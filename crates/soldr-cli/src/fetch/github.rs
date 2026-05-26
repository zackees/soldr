//! GitHub Releases + crates.io repo resolution and asset matching.
//!
//! Extracted from `fetch/mod.rs` during the >1k-LOC refactor. The
//! retry-and-cache wrappers around these helpers live in the parent
//! module.

use crate::core::{Arch, Env, Os, SoldrError, TargetTriple};

use super::VersionSpec;

pub(crate) struct RepoInfo {
    pub(crate) owner: String,
    pub(crate) repo: String,
}

pub(super) struct ReleaseInfo {
    pub(super) version: String,
    pub(super) assets: Vec<AssetInfo>,
}

pub(super) struct AssetInfo {
    pub(super) name: String,
    pub(super) download_url: String,
}

pub(crate) fn http_client() -> Result<reqwest::Client, SoldrError> {
    reqwest::Client::builder()
        .user_agent(format!("soldr/{}", crate::core::version()))
        .build()
        .map_err(|e| SoldrError::Network(e.to_string()))
}

/// Look up the GitHub repository for a crate via crates.io.
pub(super) async fn resolve_repo(crate_name: &str) -> Result<RepoInfo, SoldrError> {
    let client = http_client()?;
    let url = format!("https://crates.io/api/v1/crates/{crate_name}");

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(SoldrError::ToolNotFound(format!(
            "{crate_name}: not found on crates.io"
        )));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    let body: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| SoldrError::Other(e.to_string()))?;

    let repo_url = body["crate"]["repository"].as_str().ok_or_else(|| {
        SoldrError::ToolNotFound(format!("{crate_name}: no repository on crates.io"))
    })?;

    parse_github_url(repo_url)
}

fn parse_github_url(url: &str) -> Result<RepoInfo, SoldrError> {
    let url = url.trim_end_matches(".git").trim_end_matches('/');
    let parts: Vec<&str> = url.split('/').collect();

    let gh_idx = parts
        .iter()
        .position(|p| p.contains("github.com"))
        .ok_or_else(|| SoldrError::Other(format!("not a GitHub URL: {url}")))?;

    if parts.len() < gh_idx + 3 {
        return Err(SoldrError::Other(format!("invalid GitHub URL: {url}")));
    }

    Ok(RepoInfo {
        owner: parts[gh_idx + 1].to_string(),
        repo: parts[gh_idx + 2].to_string(),
    })
}

/// Fetch release metadata (asset list) from GitHub.
pub(super) async fn fetch_release(
    repo: &RepoInfo,
    version: &VersionSpec,
    tag_prefix: Option<&str>,
) -> Result<ReleaseInfo, SoldrError> {
    let client = http_client()?;

    let release = match version {
        VersionSpec::Latest => match tag_prefix {
            // Monorepo releases: `/releases/latest` may pick a sibling tool;
            // instead list releases and take the newest whose tag matches.
            Some(prefix) => fetch_latest_by_prefix(&client, repo, prefix).await?,
            None => {
                let url = format!(
                    "https://api.github.com/repos/{}/{}/releases/latest",
                    repo.owner, repo.repo
                );
                fetch_release_value(&client, repo, &url).await?
            }
        },
        VersionSpec::Exact(v) => {
            let candidate_tags = release_tag_candidates(v, tag_prefix);
            let mut found = None;
            for tag in &candidate_tags {
                let url = format!(
                    "https://api.github.com/repos/{}/{}/releases/tags/{tag}",
                    repo.owner, repo.repo
                );
                match fetch_release_value(&client, repo, &url).await {
                    Ok(release) => {
                        found = Some(release);
                        break;
                    }
                    Err(SoldrError::ToolNotFound(_)) => continue,
                    Err(err) => return Err(err),
                }
            }
            match found {
                Some(release) => release,
                None => fetch_release_by_listing(&client, repo, &candidate_tags).await?,
            }
        }
    };

    parse_release_info(release, tag_prefix)
}

async fn fetch_latest_by_prefix(
    client: &reqwest::Client,
    repo: &RepoInfo,
    prefix: &str,
) -> Result<serde_json::Value, SoldrError> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases?per_page=60",
        repo.owner, repo.repo
    );
    let resp = github_request(client, &url)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(SoldrError::ToolNotFound(format!(
            "no release found for {}/{}",
            repo.owner, repo.repo
        )));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    let releases: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| SoldrError::Other(e.to_string()))?;

    let matched = releases
        .as_array()
        .and_then(|items| {
            items.iter().find(|release| {
                let is_prerelease = release["prerelease"].as_bool().unwrap_or(false);
                if is_prerelease {
                    return false;
                }
                release["tag_name"]
                    .as_str()
                    .map(|tag| tag.starts_with(prefix))
                    .unwrap_or(false)
            })
        })
        .cloned();

    matched.ok_or_else(|| {
        SoldrError::ToolNotFound(format!(
            "no release with tag prefix {prefix:?} found for {}/{}",
            repo.owner, repo.repo
        ))
    })
}

async fn fetch_release_value(
    client: &reqwest::Client,
    repo: &RepoInfo,
    url: &str,
) -> Result<serde_json::Value, SoldrError> {
    let resp = github_request(client, url)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(SoldrError::ToolNotFound(format!(
            "no release found for {}/{}",
            repo.owner, repo.repo
        )));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| SoldrError::Other(e.to_string()))
}

async fn fetch_release_by_listing(
    client: &reqwest::Client,
    repo: &RepoInfo,
    tags: &[String],
) -> Result<serde_json::Value, SoldrError> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases?per_page=30",
        repo.owner, repo.repo
    );
    let resp = github_request(client, &url)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(SoldrError::ToolNotFound(format!(
            "no release found for {}/{}",
            repo.owner, repo.repo
        )));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    let releases: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| SoldrError::Other(e.to_string()))?;
    let matched = releases
        .as_array()
        .and_then(|items| {
            items.iter().find(|release| {
                release["tag_name"]
                    .as_str()
                    .map(|release_tag| tags.iter().any(|tag| release_tag == tag))
                    .unwrap_or(false)
            })
        })
        .cloned();

    matched.ok_or_else(|| {
        SoldrError::ToolNotFound(format!("no release found for {}/{}", repo.owner, repo.repo))
    })
}

fn release_tag_candidates(version: &str, tag_prefix: Option<&str>) -> Vec<String> {
    let mut tags = Vec::with_capacity(4);
    let raw = version.trim();
    if raw.is_empty() {
        return tags;
    }
    let bare = raw.trim_start_matches('v').to_string();

    // Core bare + v-prefixed variants.
    tags.push(raw.to_string());
    if let Some(stripped) = raw.strip_prefix('v') {
        if !stripped.is_empty() {
            tags.push(stripped.to_string());
        }
    } else {
        tags.push(format!("v{raw}"));
    }

    // Monorepo-style tags: e.g. `cargo-audit/v0.21.0`.
    if let Some(prefix) = tag_prefix {
        tags.push(format!("{prefix}{bare}"));
        tags.push(format!("{prefix}v{bare}"));
    }

    tags.sort();
    tags.dedup();
    tags
}

fn parse_release_info(
    body: serde_json::Value,
    tag_prefix: Option<&str>,
) -> Result<ReleaseInfo, SoldrError> {
    let tag = body["tag_name"]
        .as_str()
        .ok_or_else(|| SoldrError::Other("no tag_name in release".into()))?;
    let stripped = match tag_prefix {
        Some(prefix) => tag.strip_prefix(prefix).unwrap_or(tag),
        None => tag,
    };
    let version = stripped.trim_start_matches('v').to_string();

    let assets = body["assets"]
        .as_array()
        .ok_or_else(|| SoldrError::Other("no assets in release".into()))?
        .iter()
        .filter_map(|a| {
            Some(AssetInfo {
                name: a["name"].as_str()?.to_string(),
                download_url: a["browser_download_url"].as_str()?.to_string(),
            })
        })
        .collect();

    Ok(ReleaseInfo { version, assets })
}

fn github_request<'a>(client: &'a reqwest::Client, url: &'a str) -> reqwest::RequestBuilder {
    let mut request = client
        .get(url)
        .header("Accept", "application/vnd.github+json");

    if let Some(token) = std::env::var("GITHUB_TOKEN")
        .ok()
        .or_else(|| std::env::var("GH_TOKEN").ok())
    {
        request = request.bearer_auth(token);
    }

    request
}

/// Pick the best asset for our target triple.
pub(super) fn match_asset<'a>(
    assets: &'a [AssetInfo],
    target: &TargetTriple,
) -> Result<&'a AssetInfo, SoldrError> {
    let os_keywords: &[&str] = match target.os {
        Os::Windows => &["windows", "win64", "win"],
        Os::MacOs => &["macos", "darwin", "apple", "osx"],
        Os::Linux => &["linux"],
    };

    let arch_keywords: &[&str] = match target.arch {
        Arch::X86_64 => &["x86_64", "amd64", "x64"],
        Arch::Aarch64 => &["aarch64", "arm64"],
    };

    let mut best: Option<(&AssetInfo, u32)> = None;

    for asset in assets {
        let name = asset.name.to_lowercase();

        // Must match OS and arch.
        if !os_keywords.iter().any(|k| name.contains(k)) {
            continue;
        }
        if !arch_keywords.iter().any(|k| name.contains(k)) {
            continue;
        }

        // Skip source archives.
        if name.contains("src") || name.contains("source") {
            continue;
        }
        if name.contains("-debug.") {
            continue;
        }

        // Respect the resolved ABI/libc instead of assuming Windows is always MSVC.
        if target.os == Os::Windows && target.env == Env::Msvc && name.contains("gnu") {
            continue;
        }
        if target.os == Os::Windows && target.env == Env::Gnu && name.contains("msvc") {
            continue;
        }
        if target.os == Os::Linux && target.env == Env::Musl && name.contains("gnu") {
            continue;
        }
        // zccache currently publishes musl Linux archives; keep those as a
        // fallback on GNU runners instead of forcing a slow cargo install.

        let mut score: u32 = 1;
        if target.os == Os::Windows && name.contains("msvc") {
            score += 10;
        }
        if target.os == Os::Windows && target.env == Env::Gnu && name.contains("gnu") {
            score += 10;
        }
        if target.os == Os::Linux && target.env == Env::Musl && name.contains("musl") {
            score += 10;
        }
        if target.os == Os::Linux && target.env == Env::Gnu && name.contains("gnu") {
            score += 10;
        }
        if name.ends_with(target.archive_ext()) {
            score += 5;
        }

        if best.is_none_or(|(_, s)| score > s) {
            best = Some((asset, score));
        }
    }

    best.map(|(a, _)| a).ok_or_else(|| {
        SoldrError::ToolNotFound(format!("no asset matches target {}", target.triple()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> AssetInfo {
        AssetInfo {
            name: name.to_string(),
            download_url: format!("https://example.com/{name}"),
        }
    }

    #[test]
    fn match_asset_prefers_windows_gnu_when_requested() {
        let assets = vec![
            asset("tool-x86_64-pc-windows-msvc.zip"),
            asset("tool-x86_64-pc-windows-gnu.zip"),
        ];
        let target = TargetTriple {
            arch: Arch::X86_64,
            os: Os::Windows,
            env: Env::Gnu,
        };

        let selected = match_asset(&assets, &target).unwrap();
        assert_eq!(selected.name, "tool-x86_64-pc-windows-gnu.zip");
    }

    #[test]
    fn match_asset_prefers_windows_msvc_when_requested() {
        let assets = vec![
            asset("tool-x86_64-pc-windows-msvc.zip"),
            asset("tool-x86_64-pc-windows-gnu.zip"),
        ];
        let target = TargetTriple {
            arch: Arch::X86_64,
            os: Os::Windows,
            env: Env::Msvc,
        };

        let selected = match_asset(&assets, &target).unwrap();
        assert_eq!(selected.name, "tool-x86_64-pc-windows-msvc.zip");
    }

    #[test]
    fn match_asset_prefers_linux_musl_when_requested() {
        let assets = vec![
            asset("tool-x86_64-unknown-linux-gnu.tar.gz"),
            asset("tool-x86_64-unknown-linux-musl.tar.gz"),
        ];
        let target = TargetTriple {
            arch: Arch::X86_64,
            os: Os::Linux,
            env: Env::Musl,
        };

        let selected = match_asset(&assets, &target).unwrap();
        assert_eq!(selected.name, "tool-x86_64-unknown-linux-musl.tar.gz");
    }

    #[test]
    fn match_asset_prefers_linux_gnu_when_both_linux_variants_exist() {
        let assets = vec![
            asset("tool-x86_64-unknown-linux-gnu.tar.gz"),
            asset("tool-x86_64-unknown-linux-musl.tar.gz"),
        ];
        let target = TargetTriple {
            arch: Arch::X86_64,
            os: Os::Linux,
            env: Env::Gnu,
        };

        let selected = match_asset(&assets, &target).unwrap();
        assert_eq!(selected.name, "tool-x86_64-unknown-linux-gnu.tar.gz");
    }

    #[test]
    fn match_asset_accepts_linux_musl_as_gnu_fallback() {
        let assets = vec![asset("tool-x86_64-unknown-linux-musl.tar.gz")];
        let target = TargetTriple {
            arch: Arch::X86_64,
            os: Os::Linux,
            env: Env::Gnu,
        };

        let selected = match_asset(&assets, &target).unwrap();
        assert_eq!(selected.name, "tool-x86_64-unknown-linux-musl.tar.gz");
    }

    #[test]
    fn match_asset_skips_debug_sidecar_archives() {
        let assets = vec![
            asset("tool-x86_64-pc-windows-msvc-debug.zip"),
            asset("tool-x86_64-pc-windows-msvc.zip"),
        ];
        let target = TargetTriple {
            arch: Arch::X86_64,
            os: Os::Windows,
            env: Env::Msvc,
        };

        let selected = match_asset(&assets, &target).unwrap();
        assert_eq!(selected.name, "tool-x86_64-pc-windows-msvc.zip");
    }

    #[test]
    fn release_tag_candidates_support_plain_and_v_prefixed_tags() {
        assert_eq!(
            release_tag_candidates("1.2.8", None),
            vec!["1.2.8".to_string(), "v1.2.8".to_string()]
        );
        assert_eq!(
            release_tag_candidates("v1.2.8", None),
            vec!["1.2.8".to_string(), "v1.2.8".to_string()]
        );
    }

    #[test]
    fn release_tag_candidates_include_monorepo_prefix_variants() {
        let candidates = release_tag_candidates("0.21.0", Some("cargo-audit/"));
        assert!(candidates.contains(&"0.21.0".to_string()));
        assert!(candidates.contains(&"v0.21.0".to_string()));
        assert!(candidates.contains(&"cargo-audit/0.21.0".to_string()));
        assert!(candidates.contains(&"cargo-audit/v0.21.0".to_string()));
    }

    #[test]
    fn parse_release_info_strips_monorepo_tag_prefix() {
        let body = serde_json::json!({
            "tag_name": "cargo-audit/v0.21.0",
            "assets": [],
        });
        let info = parse_release_info(body, Some("cargo-audit/")).unwrap();
        assert_eq!(info.version, "0.21.0");
    }

    #[test]
    fn parse_release_info_strips_nextest_prefix() {
        let body = serde_json::json!({
            "tag_name": "cargo-nextest-0.9.100",
            "assets": [],
        });
        let info = parse_release_info(body, Some("cargo-nextest-")).unwrap();
        assert_eq!(info.version, "0.9.100");
    }
}
