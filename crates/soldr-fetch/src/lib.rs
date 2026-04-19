//! Binary resolution and download for soldr.
//!
//! Resolution chain (Phase 1 MVP):
//! 1. Local cache (`~/.soldr/bin/<tool>-<version>/`)
//! 2. GitHub Releases (repository URL from crates.io, or override from `known_tools`)

pub mod known_tools;

pub use known_tools::{lookup_by_cargo_subcommand, lookup_by_crate, ToolSpec, KNOWN_TOOLS};

pub mod trust;

pub use trust::{
    sha256_of, verify_download, PinnedChecksumStore, TrustMode, VerifyOutcome,
    CHECKSUMS_FILE_ENV_VAR, TRUST_MODE_ENV_VAR,
};

use soldr_core::{Arch, Env, Os, SoldrError, SoldrPaths, TargetTriple};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum VersionSpec {
    Latest,
    Exact(String),
}

impl VersionSpec {
    pub fn parse(s: &str) -> Self {
        if s.is_empty() || s == "latest" {
            Self::Latest
        } else {
            Self::Exact(s.to_string())
        }
    }
}

#[derive(Debug)]
pub struct FetchResult {
    pub binary_path: PathBuf,
    pub version: String,
    pub cached: bool,
}

pub const MANAGED_ZCCACHE_VERSION: &str = "1.2.14";

/// Fetch a tool binary for the current platform.
pub async fn fetch_tool(
    crate_name: &str,
    version: &VersionSpec,
) -> Result<FetchResult, SoldrError> {
    let paths = SoldrPaths::new()?;
    fetch_tool_with_paths(crate_name, version, &paths).await
}

/// Fetch with explicit paths (useful for testing).
pub async fn fetch_tool_with_paths(
    crate_name: &str,
    version: &VersionSpec,
    paths: &SoldrPaths,
) -> Result<FetchResult, SoldrError> {
    paths.ensure_dirs()?;

    if let Some(spec) = lookup_by_crate(crate_name) {
        let repo = match spec.repo {
            Some((owner, name)) => RepoInfo {
                owner: owner.to_string(),
                repo: name.to_string(),
            },
            None => resolve_repo(crate_name).await?,
        };
        return fetch_repo_binary_with_paths(
            spec.crate_name,
            &[spec.binary_name],
            &repo,
            version,
            spec.tag_prefix,
            paths,
        )
        .await;
    }

    let repo = resolve_repo(crate_name).await?;
    fetch_repo_binary_with_paths(crate_name, &[crate_name], &repo, version, None, paths).await
}

pub async fn fetch_zccache() -> Result<FetchResult, SoldrError> {
    let paths = SoldrPaths::new()?;
    fetch_zccache_with_paths(&paths).await
}

pub async fn fetch_zccache_with_paths(paths: &SoldrPaths) -> Result<FetchResult, SoldrError> {
    paths.ensure_dirs()?;
    let target = TargetTriple::detect()?;
    let binary_names = ["zccache", "zccache-daemon", "zccache-fp"];

    if let Some(result) = check_cache(
        paths,
        "zccache",
        MANAGED_ZCCACHE_VERSION,
        &binary_names,
        &target,
    )? {
        return Ok(result);
    }

    let download_url = managed_zccache_download_url(MANAGED_ZCCACHE_VERSION, &target);
    let binary_path = download_and_extract(
        paths,
        "zccache",
        MANAGED_ZCCACHE_VERSION,
        &download_url,
        &target,
        &binary_names,
    )
    .await?;

    Ok(FetchResult {
        binary_path,
        version: MANAGED_ZCCACHE_VERSION.to_string(),
        cached: false,
    })
}

pub fn cached_zccache_binary(paths: &SoldrPaths) -> Result<Option<FetchResult>, SoldrError> {
    let target = TargetTriple::detect()?;
    check_cache(
        paths,
        "zccache",
        MANAGED_ZCCACHE_VERSION,
        &["zccache", "zccache-daemon", "zccache-fp"],
        &target,
    )
}

async fn fetch_repo_binary_with_paths(
    cache_name: &str,
    binary_names: &[&str],
    repo: &RepoInfo,
    version: &VersionSpec,
    tag_prefix: Option<&str>,
    paths: &SoldrPaths,
) -> Result<FetchResult, SoldrError> {
    paths.ensure_dirs()?;
    let target = TargetTriple::detect()?;
    if binary_names.is_empty() {
        return Err(SoldrError::Other(format!(
            "no binary names configured for {cache_name}"
        )));
    }

    if let VersionSpec::Exact(ref v) = version {
        if let Some(r) = check_cache(paths, cache_name, v, binary_names, &target)? {
            return Ok(r);
        }
    }

    let release = fetch_release(repo, version, tag_prefix).await?;

    if let Some(r) = check_cache(paths, cache_name, &release.version, binary_names, &target)? {
        return Ok(r);
    }

    let asset = match_asset(&release.assets, &target)?;

    let binary_path = download_and_extract(
        paths,
        cache_name,
        &release.version,
        &asset.download_url,
        &target,
        binary_names,
    )
    .await?;

    Ok(FetchResult {
        binary_path,
        version: release.version,
        cached: false,
    })
}

// ---------------------------------------------------------------------------
// Local cache
// ---------------------------------------------------------------------------

fn check_cache(
    paths: &SoldrPaths,
    cache_name: &str,
    version: &str,
    binary_names: &[&str],
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let tool_dir = paths.bin.join(format!("{cache_name}-{version}"));
    let bin_name = format!(
        "{}{}",
        binary_names
            .first()
            .ok_or_else(|| SoldrError::Other(format!(
                "no binary names configured for {cache_name}"
            )))?,
        target.binary_ext()
    );
    let binary_path = tool_dir.join(&bin_name);

    if binary_names.iter().all(|binary_name| {
        tool_dir
            .join(format!("{binary_name}{}", target.binary_ext()))
            .exists()
    }) {
        Ok(Some(FetchResult {
            binary_path,
            version: version.to_string(),
            cached: true,
        }))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// crates.io + GitHub resolution
// ---------------------------------------------------------------------------

struct RepoInfo {
    owner: String,
    repo: String,
}

struct ReleaseInfo {
    version: String,
    assets: Vec<AssetInfo>,
}

struct AssetInfo {
    name: String,
    download_url: String,
}

fn http_client() -> Result<reqwest::Client, SoldrError> {
    reqwest::Client::builder()
        .user_agent(format!("soldr/{}", soldr_core::version()))
        .build()
        .map_err(|e| SoldrError::Network(e.to_string()))
}

/// Look up the GitHub repository for a crate via crates.io.
async fn resolve_repo(crate_name: &str) -> Result<RepoInfo, SoldrError> {
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
async fn fetch_release(
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

fn managed_zccache_download_url(version: &str, target: &TargetTriple) -> String {
    format!(
        "https://github.com/zackees/zccache/releases/download/{version}/zccache-{version}-{}.tar.gz",
        target.triple()
    )
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
fn match_asset<'a>(
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
        if target.os == Os::Linux && target.env == Env::Gnu && name.contains("musl") {
            continue;
        }

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

        if best.map_or(true, |(_, s)| score > s) {
            best = Some((asset, score));
        }
    }

    best.map(|(a, _)| a).ok_or_else(|| {
        SoldrError::ToolNotFound(format!("no asset matches target {}", target.triple()))
    })
}

// ---------------------------------------------------------------------------
// Download + extract
// ---------------------------------------------------------------------------

async fn download_and_extract(
    paths: &SoldrPaths,
    cache_name: &str,
    version: &str,
    url: &str,
    target: &TargetTriple,
    binary_names: &[&str],
) -> Result<PathBuf, SoldrError> {
    let client = http_client()?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "download failed: HTTP {}",
            resp.status()
        )));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    // Integrity + trust enforcement (issue #42). Compute sha256 and consult
    // the pinned-checksum store before writing anything to disk.
    let asset_name = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(url);
    let digest = trust::sha256_of(&bytes);
    let store = trust::PinnedChecksumStore::from_env()?;
    let mode = trust::TrustMode::from_env();
    match trust::verify_download(cache_name, version, asset_name, &digest, &store, mode)? {
        trust::VerifyOutcome::Verified { sha256 } => {
            eprintln!(
                "soldr: trust: verified {cache_name} v{version} {asset_name} sha256={sha256}"
            );
        }
        trust::VerifyOutcome::Unverified { sha256 } => {
            eprintln!(
                "soldr: trust: unverified {cache_name} v{version} {asset_name} sha256={sha256} (set {} to pin; run with {}=strict to require pins)",
                trust::CHECKSUMS_FILE_ENV_VAR,
                trust::TRUST_MODE_ENV_VAR
            );
        }
    }

    let tool_dir = paths.bin.join(format!("{cache_name}-{version}"));
    let desired_binaries = desired_binary_names(binary_names, target);
    std::fs::create_dir_all(&tool_dir)?;

    let main_binary_name = desired_binaries
        .first()
        .cloned()
        .ok_or_else(|| SoldrError::Other(format!("no binary names configured for {cache_name}")))?;
    let binary_path = tool_dir.join(&main_binary_name);

    if url.ends_with(".zip") {
        extract_zip(&bytes, &tool_dir, &desired_binaries)?;
    } else if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        extract_tar_gz(&bytes, &tool_dir, &desired_binaries)?;
    } else {
        // Assume raw binary.
        if desired_binaries.len() != 1 {
            return Err(SoldrError::Archive(format!(
                "cannot extract multiple binaries from raw asset for {cache_name}"
            )));
        }
        std::fs::write(&binary_path, &bytes)?;
    }

    // Make executable on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for binary_name in &desired_binaries {
            let binary_path = tool_dir.join(binary_name);
            let mut perms = std::fs::metadata(&binary_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&binary_path, perms)?;
        }
    }

    Ok(binary_path)
}

fn desired_binary_names(binary_names: &[&str], target: &TargetTriple) -> Vec<String> {
    binary_names
        .iter()
        .map(|binary_name| format!("{binary_name}{}", target.binary_ext()))
        .collect()
}

fn extract_zip(data: &[u8], dest_dir: &Path, binary_names: &[String]) -> Result<(), SoldrError> {
    let reader = std::io::Cursor::new(data);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| SoldrError::Archive(e.to_string()))?;
    let mut found = std::collections::BTreeSet::new();

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| SoldrError::Archive(e.to_string()))?;

        if file.is_dir() {
            continue;
        }

        let file_name = Path::new(file.name())
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");

        let wanted = binary_names.iter().find(|binary_name| {
            file_name == *binary_name || file_name == binary_name.trim_end_matches(".exe")
        });

        if let Some(binary_name) = wanted {
            let mut out = std::fs::File::create(dest_dir.join(binary_name))?;
            std::io::copy(&mut file, &mut out)?;
            found.insert(binary_name.clone());
        }
    }

    ensure_all_binaries_found(binary_names, &found)
}

fn extract_tar_gz(data: &[u8], dest_dir: &Path, binary_names: &[String]) -> Result<(), SoldrError> {
    let reader = std::io::Cursor::new(data);
    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);
    let mut found = std::collections::BTreeSet::new();

    for entry in archive
        .entries()
        .map_err(|e| SoldrError::Archive(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| SoldrError::Archive(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| SoldrError::Archive(e.to_string()))?;

        let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");

        let wanted = binary_names.iter().find(|binary_name| {
            file_name == *binary_name || file_name == binary_name.trim_end_matches(".exe")
        });

        if let Some(binary_name) = wanted {
            let mut out = std::fs::File::create(dest_dir.join(binary_name))?;
            std::io::copy(&mut entry, &mut out)?;
            found.insert(binary_name.clone());
        }
    }

    ensure_all_binaries_found(binary_names, &found)
}

fn ensure_all_binaries_found(
    binary_names: &[String],
    found: &std::collections::BTreeSet<String>,
) -> Result<(), SoldrError> {
    let missing = binary_names
        .iter()
        .filter(|binary_name| !found.contains(*binary_name))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(SoldrError::Archive(format!(
            "missing binaries in archive: {}",
            missing.join(", ")
        )))
    }
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

    #[test]
    fn managed_zccache_download_url_uses_target_triple() {
        let target = TargetTriple {
            arch: Arch::Aarch64,
            os: Os::MacOs,
            env: Env::None,
        };
        assert_eq!(
            managed_zccache_download_url("1.2.8", &target),
            "https://github.com/zackees/zccache/releases/download/1.2.8/zccache-1.2.8-aarch64-apple-darwin.tar.gz"
        );
    }
}
