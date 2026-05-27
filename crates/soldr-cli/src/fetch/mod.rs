//! Binary resolution and download for soldr.
//!
//! Resolution chain (Phase 1 MVP):
//! 1. Local cache (`~/.soldr/bin/<tool>-<version>/`)
//! 2. GitHub Releases (repository URL from crates.io, or override from `known_tools`)

pub mod known_tools;

pub use known_tools::{
    known_cargo_subcommands, lookup_by_cargo_subcommand, lookup_by_crate, ToolSpec,
    CARGO_CHEF_PINNED_VERSION, KNOWN_TOOLS,
};

pub mod trust;

pub use trust::{
    sha256_of, verify_download, PinnedChecksumStore, TrustMode, VerifyOutcome,
    CHECKSUMS_FILE_ENV_VAR, TRUST_MODE_ENV_VAR,
};

pub mod install_zccache;

pub use install_zccache::{
    install_zccache_from_source, pinned_version_drift_from_managed, pinned_zccache_dir,
    read_pinned_sidecar, remove_pinned_zccache, resolve_pinned_zccache, InstallReport,
    InstallSource, PinnedBinaryRecord, PinnedResolution, PinnedSidecar, PINNED_ZCCACHE_DIRNAME,
    PINNED_ZCCACHE_SIDECAR_FILENAME, PINNED_ZCCACHE_SIDECAR_SCHEMA_VERSION,
    ZCCACHE_PINNED_BINARY_NAMES,
};

pub mod rustup_init;

pub use rustup_init::{
    auto_bootstrap_if_missing, auto_bootstrap_if_missing_blocking, bootstrap_rustup,
    bootstrap_rustup_blocking, discover_rustup, managed_cargo_home, managed_rustup_home,
    managed_rustup_path, rustup_init_download_url, rustup_init_host_triple, AutoBootstrapOutcome,
    BootstrapReport, NO_BOOTSTRAP_ENV_VAR, RUSTUP_INIT_TRIPLE_ENV_VAR, RUSTUP_INIT_URL_ENV_VAR,
};

pub mod archive;
pub mod github;
pub mod zccache;
pub mod zccache_install;
pub mod zccache_runtime;

#[cfg(test)]
mod zccache_contract_tests;

pub use zccache::{
    classify_zccache_source, managed_only_zccache_summary, pinned_zccache_summary,
    resolve_local_zccache, zccache_binary_summary, ZccacheBinarySummary, ZccacheSource,
};
pub use zccache_install::{
    cached_zccache_binary, fetch_zccache_with_paths, resolve_system_zccache,
};
pub use zccache_runtime::{ZccacheResolver, ZccacheRuntime, ZccacheRuntimeSource};

pub(crate) use github::http_client;

use crate::core::{SoldrError, SoldrPaths, TargetTriple};
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

pub const MANAGED_ZCCACHE_VERSION: &str = "1.11.5";
// After the Wave 7 monocrate rename in zccache (`zccache-monocrate` -> `zccache`),
// all three native binaries (`zccache`, `zccache-daemon`, `zccache-fp`) are
// `[[bin]]` targets inside the umbrella `zccache` crate on crates.io. The
// sibling crates `zccache-cli`, `zccache-watcher`, and `zccache-fingerprint`
// are pyo3 cdylibs only; `cargo install` rejects them with
// "no bin target named X in <pkg>" when soldr asks for an executable.
// Each tuple is `(crates.io package, --bin name)`.
pub(crate) const MANAGED_ZCCACHE_PACKAGES: [(&str, &str); 3] = [
    ("zccache", "zccache"),
    ("zccache", "zccache-daemon"),
    ("zccache", "zccache-fp"),
];

/// Override the managed-zccache resolution entirely: instead of
/// fetching from GitHub Releases (or installing from crates.io), use
/// the locally-built binaries in this directory. See
/// `resolve_local_zccache` for the resolution contract.
pub const ZCCACHE_LOCAL_DIR_ENV_VAR: &str = "SOLDR_ZCCACHE_LOCAL_DIR";

/// Pinned crgx version that soldr's release pipeline source-builds and
/// bundles into the combined `.tar.zst` archive (see PR follow-up to
/// #434 — combined archive now ships zccache + crgx). Also surfaced
/// via `lookup_by_crate("crgx").pinned_version` so the
/// `fetch_repo_binary` fallback resolves the same version when the
/// bundled binary is not available.
pub const MANAGED_CRGX_VERSION: &str = "0.1.0";

/// Override the runtime crgx resolution: when set, soldr uses the
/// `crgx` (or `crgx.exe`) binary in this directory ahead of any
/// GitHub-Releases / crates.io fetch. The npm shim (`bin/soldr.js`)
/// and the setup-soldr action point this at the directory containing
/// the bundled binary so the first `soldr crgx ...` call needs no
/// network round trip. Mirrors `SOLDR_ZCCACHE_LOCAL_DIR`.
pub const CRGX_LOCAL_DIR_ENV_VAR: &str = "SOLDR_CRGX_LOCAL_DIR";

/// Retry budget for the GitHub-Releases + crates.io fetch chain inside
/// `fetch_repo_binary_with_paths`. Transient errors
/// (`SoldrError::Network`, `SoldrError::ToolNotFound`) retry up to
/// `REPO_FETCH_ATTEMPTS` times with exponential backoff starting at
/// `REPO_FETCH_INITIAL_BACKOFF`. The same values previously lived as
/// `MANAGED_ZCCACHE_FETCH_*` constants used at a single call site; they
/// now apply to every fetch in soldr (zccache, crgx, cargo-chef, every
/// ecosystem tool) so users and CI don't have to babysit transient
/// GitHub API hiccups.
const REPO_FETCH_ATTEMPTS: u32 = 4;
const REPO_FETCH_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const MANAGED_ZCCACHE_INSTALL_ATTEMPTS: u32 = 3;
pub(crate) const MANAGED_ZCCACHE_INSTALL_INITIAL_BACKOFF: std::time::Duration =
    std::time::Duration::from_secs(10);

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

    // Bundled-crgx escape hatch: when the npm shim or setup-soldr
    // action ships a bundled crgx (see release-auto.yml's "Build crgx
    // from pinned source" step) they export SOLDR_CRGX_LOCAL_DIR
    // pointing at the directory holding the binary. Honor it ahead of
    // every other resolution path so first-use carries no network
    // round trip. Mirrors the SOLDR_ZCCACHE_LOCAL_DIR contract used by
    // the bundled zccache trio (#434).
    if crate_name == "crgx" {
        if let Some(local_dir) = non_empty_env_path(CRGX_LOCAL_DIR_ENV_VAR) {
            return resolve_local_crgx(&local_dir);
        }
    }

    if let Some(spec) = lookup_by_crate(crate_name) {
        let repo = match spec.repo {
            Some((owner, name)) => github::RepoInfo {
                owner: owner.to_string(),
                repo: name.to_string(),
            },
            None => github::resolve_repo(crate_name).await?,
        };
        // Honor `spec.pinned_version` when the caller didn't pass an
        // explicit `@version` — mirrors the cargo-front-door's
        // existing pin substitution so `soldr crgx` and
        // `soldr cargo chef` resolve the same way from both entry
        // points. Lets the `External` dispatch in main.rs go through
        // the registry pin without each call site re-implementing it.
        let effective_version = match (version, spec.pinned_version) {
            (VersionSpec::Latest, Some(pin)) => VersionSpec::Exact(pin.to_string()),
            _ => version.clone(),
        };
        return fetch_repo_binary_with_paths(
            spec.crate_name,
            &[spec.binary_name],
            &repo,
            &effective_version,
            spec.tag_prefix,
            paths,
        )
        .await;
    }

    let repo = github::resolve_repo(crate_name).await?;
    fetch_repo_binary_with_paths(crate_name, &[crate_name], &repo, version, None, paths).await
}

/// Resolve crgx to the binary in `local_dir` set via
/// `SOLDR_CRGX_LOCAL_DIR`. Returns the path verbatim — no copy, no
/// content hashing — because crgx is a single binary with no
/// daemon/sidecar siblings (contrast with `resolve_local_zccache`
/// which normalizes a trio plus debug-info sidecars).
fn resolve_local_crgx(local_dir: &Path) -> Result<FetchResult, SoldrError> {
    if !local_dir.is_dir() {
        return Err(SoldrError::Other(format!(
            "{}={} is not a directory",
            CRGX_LOCAL_DIR_ENV_VAR,
            local_dir.display(),
        )));
    }
    let binary_name = if cfg!(windows) { "crgx.exe" } else { "crgx" };
    let binary = local_dir.join(binary_name);
    if !binary.is_file() {
        return Err(SoldrError::Other(format!(
            "{}={} but {} is missing",
            CRGX_LOCAL_DIR_ENV_VAR,
            local_dir.display(),
            binary.display(),
        )));
    }
    Ok(FetchResult {
        binary_path: binary,
        version: format!("local-{MANAGED_CRGX_VERSION}"),
        cached: true,
    })
}

pub async fn fetch_zccache() -> Result<FetchResult, SoldrError> {
    let paths = SoldrPaths::new()?;
    fetch_zccache_with_paths(&paths).await
}

pub(crate) fn non_empty_env_path(env_var: &str) -> Option<PathBuf> {
    let value = std::env::var_os(env_var)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

pub(super) async fn fetch_repo_binary_with_paths(
    cache_name: &str,
    binary_names: &[&str],
    repo: &github::RepoInfo,
    version: &VersionSpec,
    tag_prefix: Option<&str>,
    paths: &SoldrPaths,
) -> Result<FetchResult, SoldrError> {
    let mut backoff = REPO_FETCH_INITIAL_BACKOFF;
    let mut attempt: u32 = 1;
    loop {
        match fetch_repo_binary_once(cache_name, binary_names, repo, version, tag_prefix, paths)
            .await
        {
            Ok(result) => return Ok(result),
            Err(err) if attempt < REPO_FETCH_ATTEMPTS && is_transient_fetch_error(&err) => {
                eprintln!(
                    "soldr: transient error fetching {cache_name} from {}/{} (attempt {attempt}/{}): {err}; retrying in {:?}",
                    repo.owner, repo.repo, REPO_FETCH_ATTEMPTS, backoff,
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2);
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Classify whether an error from the repo-fetch chain is worth retrying.
/// `ToolNotFound` covers GitHub-Releases 404s during release propagation
/// windows + crates.io index-not-ready windows; `Network` covers DNS
/// blips, connection resets, and TLS handshake failures. Anything else
/// (malformed JSON, archive extraction errors, IO errors) is treated as
/// terminal — retrying wouldn't help.
fn is_transient_fetch_error(err: &SoldrError) -> bool {
    matches!(err, SoldrError::ToolNotFound(_) | SoldrError::Network(_))
}

/// One attempt at the full repo-binary fetch pipeline. Cache check →
/// release lookup → asset match → download + extract. Wrapped in
/// `fetch_repo_binary_with_paths`'s retry loop so transient failures
/// don't bubble up to the user.
async fn fetch_repo_binary_once(
    cache_name: &str,
    binary_names: &[&str],
    repo: &github::RepoInfo,
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

    let release = github::fetch_release(repo, version, tag_prefix).await?;

    if let Some(r) = check_cache(paths, cache_name, &release.version, binary_names, &target)? {
        return Ok(r);
    }

    let asset = github::match_asset(&release.assets, &target)?;

    let binary_path = archive::download_and_extract(
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

pub(super) fn check_cache(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_fetch_predicate_only_retries_network_and_not_found() {
        // `is_transient_fetch_error` gates the retry loop inside
        // `fetch_repo_binary_with_paths`. The two transient classes —
        // GitHub-Releases 404 during propagation, network hiccups — must
        // retry; everything else (malformed archive, missing asset for
        // the target triple, IO errors) is terminal.
        assert!(is_transient_fetch_error(&SoldrError::ToolNotFound(
            "no release found for yfedoseev/crgx".into(),
        )));
        assert!(is_transient_fetch_error(&SoldrError::Network(
            "github api unavailable".into(),
        )));
        assert!(!is_transient_fetch_error(&SoldrError::Archive(
            "corrupt archive".into(),
        )));
        assert!(!is_transient_fetch_error(&SoldrError::Other(
            "no asset matches target x86_64-pc-windows-msvc".into(),
        )));
        assert!(!is_transient_fetch_error(&SoldrError::UnsupportedPlatform(
            "wasm32".into(),
        )));
    }

    // ── Bundled crgx (SOLDR_CRGX_LOCAL_DIR) ─────────────────────────
    // Locks the contract used by the npm shim and setup-soldr action:
    // when the env var points at a directory containing `crgx`
    // (`crgx.exe` on Windows), `resolve_local_crgx` returns that path
    // verbatim with `cached: true` and a `local-<version>` label.

    fn crgx_binary_name() -> &'static str {
        if cfg!(windows) {
            "crgx.exe"
        } else {
            "crgx"
        }
    }

    #[test]
    fn resolve_local_crgx_returns_binary_path() {
        let tmp = tempfile::tempdir().unwrap();
        let stub = tmp.path().join(crgx_binary_name());
        std::fs::write(&stub, b"stub").unwrap();

        let result = resolve_local_crgx(tmp.path()).expect("should resolve");
        assert_eq!(result.binary_path, stub);
        assert!(result.cached, "bundled path must report cached=true");
        assert!(
            result.version.starts_with("local-"),
            "version should be `local-<ver>`, got: {}",
            result.version
        );
        assert!(
            result.version.contains(MANAGED_CRGX_VERSION),
            "version should embed MANAGED_CRGX_VERSION, got: {}",
            result.version
        );
    }

    #[test]
    fn resolve_local_crgx_errors_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("not-a-dir");

        let err = resolve_local_crgx(&nonexistent).expect_err("missing dir should error");
        let msg = err.to_string();
        assert!(
            msg.contains("not a directory"),
            "error must explain the dir is missing, got: {msg}"
        );
    }

    #[test]
    fn resolve_local_crgx_errors_when_binary_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty directory — no crgx binary inside.

        let err = resolve_local_crgx(tmp.path()).expect_err("missing binary should error");
        let msg = err.to_string();
        assert!(
            msg.contains("is missing"),
            "error must explain the binary is missing, got: {msg}"
        );
        assert!(
            msg.contains(crgx_binary_name()),
            "error should name the expected binary, got: {msg}"
        );
    }
}
