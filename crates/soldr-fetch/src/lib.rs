//! Binary resolution and download for soldr.
//!
//! Resolution chain (Phase 1 MVP):
//! 1. Local cache (`~/.soldr/bin/<tool>-<version>/`)
//! 2. GitHub Releases (repository URL from crates.io)

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
    let target = TargetTriple::detect()?;

    // 1. Check local cache (exact version only)
    if let VersionSpec::Exact(ref v) = version {
        if let Some(r) = check_cache(paths, crate_name, v, &target)? {
            return Ok(r);
        }
    }

    // 2. Resolve repository from crates.io
    let repo = resolve_repo(crate_name).await?;

    // 3. Get release metadata from GitHub
    let release = fetch_release(&repo, version).await?;

    // 4. Check cache for the resolved version (handles Latest -> concrete version)
    if let Some(r) = check_cache(paths, crate_name, &release.version, &target)? {
        return Ok(r);
    }

    // 5. Find matching asset for our target
    let asset = match_asset(&release.assets, &target)?;

    // 6. Download and extract
    let binary_path = download_and_extract(
        paths,
        crate_name,
        &release.version,
        &asset.download_url,
        &target,
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
    crate_name: &str,
    version: &str,
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let bin_name = format!("{crate_name}{}", target.binary_ext());
    let tool_dir = paths.bin.join(format!("{crate_name}-{version}"));
    let binary_path = tool_dir.join(&bin_name);

    if binary_path.exists() {
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
async fn fetch_release(repo: &RepoInfo, version: &VersionSpec) -> Result<ReleaseInfo, SoldrError> {
    let client = http_client()?;

    let release = match version {
        VersionSpec::Latest => {
            let url = format!(
                "https://api.github.com/repos/{}/{}/releases/latest",
                repo.owner, repo.repo
            );
            fetch_release_value(&client, repo, &url).await?
        }
        VersionSpec::Exact(v) => {
            let tag = if v.starts_with('v') {
                v.clone()
            } else {
                format!("v{v}")
            };
            let url = format!(
                "https://api.github.com/repos/{}/{}/releases/tags/{tag}",
                repo.owner, repo.repo
            );
            match fetch_release_value(&client, repo, &url).await {
                Ok(release) => release,
                Err(SoldrError::ToolNotFound(_)) => {
                    fetch_release_by_listing(&client, repo, &tag).await?
                }
                Err(err) => return Err(err),
            }
        }
    };

    parse_release_info(release)
}

async fn fetch_release_value(
    client: &reqwest::Client,
    repo: &RepoInfo,
    url: &str,
) -> Result<serde_json::Value, SoldrError> {
    let resp = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
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
    tag: &str,
) -> Result<serde_json::Value, SoldrError> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases?per_page=30",
        repo.owner, repo.repo
    );
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
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
                    .map(|release_tag| release_tag == tag)
                    .unwrap_or(false)
            })
        })
        .cloned();

    matched.ok_or_else(|| {
        SoldrError::ToolNotFound(format!("no release found for {}/{}", repo.owner, repo.repo))
    })
}

fn parse_release_info(body: serde_json::Value) -> Result<ReleaseInfo, SoldrError> {
    let tag = body["tag_name"]
        .as_str()
        .ok_or_else(|| SoldrError::Other("no tag_name in release".into()))?;
    let version = tag.trim_start_matches('v').to_string();

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
    crate_name: &str,
    version: &str,
    url: &str,
    target: &TargetTriple,
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

    let tool_dir = paths.bin.join(format!("{crate_name}-{version}"));
    std::fs::create_dir_all(&tool_dir)?;

    let bin_name = format!("{crate_name}{}", target.binary_ext());
    let binary_path = tool_dir.join(&bin_name);

    if url.ends_with(".zip") {
        extract_zip(&bytes, &binary_path, &bin_name)?;
    } else if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        extract_tar_gz(&bytes, &binary_path, &bin_name)?;
    } else {
        // Assume raw binary.
        std::fs::write(&binary_path, &bytes)?;
    }

    // Make executable on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&binary_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&binary_path, perms)?;
    }

    Ok(binary_path)
}

fn extract_zip(data: &[u8], dest: &Path, bin_name: &str) -> Result<(), SoldrError> {
    let reader = std::io::Cursor::new(data);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| SoldrError::Archive(e.to_string()))?;

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

        if file_name == bin_name || file_name == bin_name.trim_end_matches(".exe") {
            let mut out = std::fs::File::create(dest)?;
            std::io::copy(&mut file, &mut out)?;
            return Ok(());
        }
    }

    Err(SoldrError::Archive(format!(
        "{bin_name} not found in zip archive"
    )))
}

fn extract_tar_gz(data: &[u8], dest: &Path, bin_name: &str) -> Result<(), SoldrError> {
    let reader = std::io::Cursor::new(data);
    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);
    let base_name = bin_name.trim_end_matches(".exe");

    for entry in archive
        .entries()
        .map_err(|e| SoldrError::Archive(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| SoldrError::Archive(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| SoldrError::Archive(e.to_string()))?;

        let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");

        if file_name == bin_name || file_name == base_name {
            let mut out = std::fs::File::create(dest)?;
            std::io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }

    Err(SoldrError::Archive(format!(
        "{bin_name} not found in tar.gz archive"
    )))
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
}
