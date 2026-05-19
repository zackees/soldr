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

use soldr_core::{
    suppress_windows_console_window, Arch, Env, Os, SoldrError, SoldrPaths, TargetTriple,
};
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

pub const MANAGED_ZCCACHE_VERSION: &str = "1.6.0";
const MANAGED_ZCCACHE_PACKAGES: [(&str, &str); 3] = [
    ("zccache-cli", "zccache"),
    ("zccache-daemon", "zccache-daemon"),
    ("zccache-fingerprint", "zccache-fp"),
];

/// Override the managed-zccache resolution entirely: instead of
/// fetching from GitHub Releases (or installing from crates.io), use
/// the locally-built binaries in this directory. See
/// `resolve_local_zccache` for the resolution contract.
pub const ZCCACHE_LOCAL_DIR_ENV_VAR: &str = "SOLDR_ZCCACHE_LOCAL_DIR";

const MANAGED_ZCCACHE_FETCH_ATTEMPTS: u32 = 4;
const MANAGED_ZCCACHE_FETCH_INITIAL_BACKOFF: std::time::Duration =
    std::time::Duration::from_secs(5);
const MANAGED_ZCCACHE_INSTALL_ATTEMPTS: u32 = 3;
const MANAGED_ZCCACHE_INSTALL_INITIAL_BACKOFF: std::time::Duration =
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

fn non_empty_env_path(env_var: &str) -> Option<PathBuf> {
    let value = std::env::var_os(env_var)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

/// Summary of where soldr's managed zccache binaries live and what
/// source produced them. Surfaced by `soldr doctor` so users debugging
/// daemon hangs can paste the `symbol_path` into `cdb -y <path>` or
/// `_NT_SYMBOL_PATH`.
#[derive(Debug, Clone)]
pub struct ZccacheBinarySummary {
    /// Where the binaries came from: `managed`, `local`, or `none`.
    pub source: ZccacheSource,
    /// Version label. `MANAGED_ZCCACHE_VERSION` for managed builds,
    /// `local-<hash>` for `SOLDR_ZCCACHE_LOCAL_DIR` builds, empty
    /// when nothing is fetched yet.
    pub version: String,
    /// Directory whose binaries are actually executed.
    pub runtime_dir: PathBuf,
    /// Source directory for local builds (where the user's
    /// `target/release` lives). `None` for managed builds.
    pub source_dir: Option<PathBuf>,
    /// Absolute path to the active CLI binary, if present.
    pub cli_path: Option<PathBuf>,
    /// Absolute path to the active daemon binary, if present.
    pub daemon_path: Option<PathBuf>,
    /// Absolute path to the active fingerprint binary, if present.
    pub fp_path: Option<PathBuf>,
    /// Number of debug-info sidecars present next to the resolved
    /// binaries (PDBs on Windows, DWPs on Linux, dSYMs on macOS).
    pub debug_info_found: usize,
    /// Number of binaries we expect to have debug-info for (always 3).
    pub debug_info_expected: usize,
    /// Path to pass to `cdb -y` / `_NT_SYMBOL_PATH` when attaching.
    pub symbol_path: PathBuf,
}

/// Where the resolved zccache binaries came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZccacheSource {
    /// Fetched from GitHub Releases or installed via `cargo install`
    /// (the standard managed path).
    Managed,
    /// Resolved from `SOLDR_ZCCACHE_LOCAL_DIR`.
    Local,
    /// Nothing fetched yet — managed path, no binaries on disk.
    None,
}

impl ZccacheSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ZccacheSource::Managed => "managed",
            ZccacheSource::Local => "local",
            ZccacheSource::None => "none",
        }
    }
}

/// Discover the local zccache build that `SOLDR_ZCCACHE_LOCAL_DIR`
/// points at, copy the binaries (and any debug-info sidecars) into
/// the soldr-owned cache dir under a content-hashed version key, and
/// return the runtime paths.
///
/// Returns an error when the env-var dir does not exist or is missing
/// any of `zccache`, `zccache-daemon`, or `zccache-fp` (with platform
/// extension). PDB / DWP / dSYM sidecars are best-effort — missing
/// debug-info is not an error.
pub fn resolve_local_zccache(
    local_dir: &Path,
    paths: &SoldrPaths,
) -> Result<FetchResult, SoldrError> {
    let target = TargetTriple::detect()?;
    resolve_local_zccache_for_target(local_dir, paths, &target)
}

pub(crate) fn resolve_local_zccache_for_target(
    local_dir: &Path,
    paths: &SoldrPaths,
    target: &TargetTriple,
) -> Result<FetchResult, SoldrError> {
    paths.ensure_dirs()?;

    if !local_dir.exists() {
        return Err(SoldrError::Other(format!(
            "{ZCCACHE_LOCAL_DIR_ENV_VAR}={} does not exist",
            local_dir.display()
        )));
    }
    if !local_dir.is_dir() {
        return Err(SoldrError::Other(format!(
            "{ZCCACHE_LOCAL_DIR_ENV_VAR}={} is not a directory",
            local_dir.display()
        )));
    }

    // Locate every required binary up front so a missing daemon / fp
    // doesn't get reported only after we've started copying the cli.
    let binary_ext = target.binary_ext();
    let mut sources: Vec<(String, PathBuf)> = Vec::with_capacity(MANAGED_ZCCACHE_PACKAGES.len());
    for (_, binary_name) in MANAGED_ZCCACHE_PACKAGES {
        let file_name = format!("{binary_name}{binary_ext}");
        let candidate = local_dir.join(&file_name);
        if !candidate.exists() {
            return Err(SoldrError::Other(format!(
                "{ZCCACHE_LOCAL_DIR_ENV_VAR}: expected {} at {}",
                file_name,
                candidate.display()
            )));
        }
        sources.push((file_name, candidate));
    }

    // Derive a content-addressed version label so multiple local
    // builds coexist (and so the runtime copy invalidates whenever
    // the user rebuilds zccache).
    let version = local_zccache_version_label(&sources[0].1);
    let tool_dir = paths.bin.join(format!("zccache-{version}"));
    std::fs::create_dir_all(&tool_dir)?;

    for (file_name, src) in &sources {
        let dst = tool_dir.join(file_name);
        copy_if_changed(src, &dst)?;
        copy_debug_info_sidecars(src, &dst)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dst)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dst, perms)?;
        }
    }

    let binary_path = tool_dir.join(&sources[0].0);
    eprintln!(
        "soldr: using local zccache from {} (version={version})",
        local_dir.display()
    );
    Ok(FetchResult {
        binary_path,
        version,
        cached: false,
    })
}

/// Build a short content-hash label for a local zccache build. The
/// label is stable across invocations as long as the binary bytes
/// don't change, so re-running soldr doesn't re-copy. When hashing
/// fails (read error, etc.), fall back to the literal `"local"` so
/// the user still gets a working override.
fn local_zccache_version_label(binary: &Path) -> String {
    match std::fs::read(binary) {
        Ok(bytes) => format!("local-{}", sha256_short(&bytes)),
        Err(err) => {
            eprintln!(
                "soldr: failed to hash local zccache binary {}: {err}; using bare \"local\" label",
                binary.display()
            );
            "local".to_string()
        }
    }
}

fn sha256_short(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let hex_digest = hex::encode(digest);
    hex_digest[..12].to_string()
}

fn copy_if_changed(src: &Path, dst: &Path) -> Result<(), SoldrError> {
    // Cheap no-op when sizes match — content hash already pins the
    // destination dir name, so a byte-for-byte difference at the
    // same size is impossible under our naming scheme.
    if let (Ok(src_md), Ok(dst_md)) = (std::fs::metadata(src), std::fs::metadata(dst)) {
        if src_md.len() == dst_md.len() && src_md.is_file() && dst_md.is_file() {
            return Ok(());
        }
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

fn copy_debug_info_sidecars(src_binary: &Path, dst_binary: &Path) -> Result<(), SoldrError> {
    // Windows: <binary>.pdb (sibling file).
    // Linux: <binary>.dwp (sibling file).
    // macOS: <binary>.dSYM (sibling directory).
    for sidecar_ext in ["pdb", "dwp"] {
        if let Some(src) = adjacent_with_extension(src_binary, sidecar_ext) {
            if src.is_file() {
                if let Some(dst) = adjacent_with_extension(dst_binary, sidecar_ext) {
                    copy_if_changed(&src, &dst)?;
                }
            }
        }
    }
    if let Some(src) = adjacent_with_extension(src_binary, "dSYM") {
        if src.is_dir() {
            if let Some(dst) = adjacent_with_extension(dst_binary, "dSYM") {
                copy_dir_recursive(&src, &dst)?;
            }
        }
    }
    Ok(())
}

fn adjacent_with_extension(binary: &Path, ext: &str) -> Option<PathBuf> {
    let stem = binary.file_stem()?.to_owned();
    let parent = binary.parent()?;
    let mut name = stem;
    name.push(".");
    name.push(ext);
    Some(parent.join(name))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), SoldrError> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            copy_if_changed(&src_path, &dst_path)?;
        }
        // Symlinks intentionally skipped — debug-info bundles don't
        // need them and they're awkward on Windows.
    }
    Ok(())
}

/// Inspect where the managed zccache binaries live, for use by
/// `soldr doctor`. Honors `SOLDR_ZCCACHE_LOCAL_DIR` so the
/// debug-info path is surfaced even when the user is overriding the
/// managed fetch.
///
/// When the local-dir override is active, performs the local-build
/// copy (idempotent — content-hashed, copy_if_changed-protected) so
/// doctor's reported `cli_path` / `daemon_path` / PDB count reflect
/// the actually-resolvable state, not a prediction. This matches what
/// the next `soldr cargo ...` invocation will produce.
pub fn zccache_binary_summary(paths: &SoldrPaths) -> Result<ZccacheBinarySummary, SoldrError> {
    let target = TargetTriple::detect()?;

    if let Some(source_dir) = non_empty_env_path(ZCCACHE_LOCAL_DIR_ENV_VAR) {
        // Idempotent: short-circuits when the destination already
        // matches the source bytes.
        let fetch = resolve_local_zccache_for_target(&source_dir, paths, &target)?;
        let runtime_dir = fetch
            .binary_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| paths.bin.clone());
        let (cli, daemon, fp) = canonical_zccache_paths(&runtime_dir, &target);
        let (debug_found, debug_expected) =
            count_debug_info_sidecars(&[cli.as_path(), daemon.as_path(), fp.as_path()]);
        return Ok(ZccacheBinarySummary {
            source: ZccacheSource::Local,
            version: fetch.version,
            symbol_path: runtime_dir.clone(),
            runtime_dir,
            source_dir: Some(source_dir),
            cli_path: cli.exists().then_some(cli),
            daemon_path: daemon.exists().then_some(daemon),
            fp_path: fp.exists().then_some(fp),
            debug_info_found: debug_found,
            debug_info_expected: debug_expected,
        });
    }

    // Managed path: inspect the cached version directory if it exists.
    let runtime_dir = paths.bin.join(format!("zccache-{MANAGED_ZCCACHE_VERSION}"));
    let (cli, daemon, fp) = canonical_zccache_paths(&runtime_dir, &target);
    let any_present = cli.exists() || daemon.exists() || fp.exists();
    let (debug_found, debug_expected) =
        count_debug_info_sidecars(&[cli.as_path(), daemon.as_path(), fp.as_path()]);
    Ok(ZccacheBinarySummary {
        source: if any_present {
            ZccacheSource::Managed
        } else {
            ZccacheSource::None
        },
        version: if any_present {
            MANAGED_ZCCACHE_VERSION.to_string()
        } else {
            String::new()
        },
        symbol_path: runtime_dir.clone(),
        runtime_dir,
        source_dir: None,
        cli_path: cli.exists().then_some(cli),
        daemon_path: daemon.exists().then_some(daemon),
        fp_path: fp.exists().then_some(fp),
        debug_info_found: debug_found,
        debug_info_expected: debug_expected,
    })
}

fn canonical_zccache_paths(
    runtime_dir: &Path,
    target: &TargetTriple,
) -> (PathBuf, PathBuf, PathBuf) {
    let ext = target.binary_ext();
    (
        runtime_dir.join(format!("zccache{ext}")),
        runtime_dir.join(format!("zccache-daemon{ext}")),
        runtime_dir.join(format!("zccache-fp{ext}")),
    )
}

fn count_debug_info_sidecars(binaries: &[&Path]) -> (usize, usize) {
    let mut found = 0usize;
    for binary in binaries {
        if !binary.exists() {
            continue;
        }
        let mut sidecar_found = false;
        for ext in ["pdb", "dwp"] {
            if let Some(sidecar) = adjacent_with_extension(binary, ext) {
                if sidecar.is_file() {
                    sidecar_found = true;
                    break;
                }
            }
        }
        if !sidecar_found {
            if let Some(sidecar) = adjacent_with_extension(binary, "dSYM") {
                if sidecar.is_dir() {
                    sidecar_found = true;
                }
            }
        }
        if sidecar_found {
            found += 1;
        }
    }
    (found, binaries.len())
}

pub async fn fetch_zccache_with_paths(paths: &SoldrPaths) -> Result<FetchResult, SoldrError> {
    paths.ensure_dirs()?;
    let target = TargetTriple::detect()?;

    // Local-build override (issue: zccache #276 daemon-stdio hang).
    // When set, skip the managed fetch entirely and copy the user's
    // locally-built binaries (+ PDBs) into the soldr cache so cdb /
    // WinDbg can resolve symbols when attaching to the daemon.
    if let Some(local_dir) = non_empty_env_path(ZCCACHE_LOCAL_DIR_ENV_VAR) {
        return resolve_local_zccache_for_target(&local_dir, paths, &target);
    }

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

    let release_version = VersionSpec::Exact(MANAGED_ZCCACHE_VERSION.to_string());
    // A version bump that merges before the upstream GitHub release and the
    // crates.io index have fully propagated will see a brief window where both
    // sources return "not found". Retry the release fetch with backoff so the
    // window can pass before we fall back to cargo install.
    let repo = managed_zccache_repo();
    let mut backoff = MANAGED_ZCCACHE_FETCH_INITIAL_BACKOFF;
    let mut attempt = 1u32;
    let release_outcome = loop {
        match fetch_repo_binary_with_paths(
            "zccache",
            &binary_names,
            &repo,
            &release_version,
            None,
            paths,
        )
        .await
        {
            Ok(result) => break Ok(result),
            Err(err)
                if attempt < MANAGED_ZCCACHE_FETCH_ATTEMPTS
                    && should_retry_managed_zccache_fetch(&err) =>
            {
                eprintln!(
                    "soldr: transient error fetching managed zccache (attempt {attempt}/{}): {err}; retrying in {:?}",
                    MANAGED_ZCCACHE_FETCH_ATTEMPTS, backoff
                );
                std::thread::sleep(backoff);
                backoff = backoff.saturating_mul(2);
                attempt += 1;
            }
            Err(err) => break Err(err),
        }
    };

    match release_outcome {
        Ok(result) => return Ok(result),
        Err(err) if should_fallback_to_managed_zccache_cargo_install(&err) => {
            eprintln!(
                "soldr: managed zccache prebuilt unavailable ({err}); falling back to cargo install"
            );
        }
        Err(err) => return Err(err),
    }

    let binary_path = install_zccache_from_crates_io(paths, MANAGED_ZCCACHE_VERSION, &target)?;

    Ok(FetchResult {
        binary_path,
        version: MANAGED_ZCCACHE_VERSION.to_string(),
        cached: false,
    })
}

pub fn cached_zccache_binary(paths: &SoldrPaths) -> Result<Option<FetchResult>, SoldrError> {
    let target = TargetTriple::detect()?;

    // When SOLDR_ZCCACHE_LOCAL_DIR is set, surface the local build as
    // the cached result so doctor / cache status can find it without
    // forcing a copy. resolve_local_zccache_for_target is idempotent:
    // it short-circuits when the destination already matches the
    // source bytes.
    if let Some(local_dir) = non_empty_env_path(ZCCACHE_LOCAL_DIR_ENV_VAR) {
        return resolve_local_zccache_for_target(&local_dir, paths, &target).map(Some);
    }

    check_cache(
        paths,
        "zccache",
        MANAGED_ZCCACHE_VERSION,
        &["zccache", "zccache-daemon", "zccache-fp"],
        &target,
    )
}

fn managed_zccache_repo() -> RepoInfo {
    RepoInfo {
        owner: "zackees".to_string(),
        repo: "zccache".to_string(),
    }
}

fn should_fallback_to_managed_zccache_cargo_install(error: &SoldrError) -> bool {
    matches!(error, SoldrError::ToolNotFound(_) | SoldrError::Network(_))
}

fn should_retry_managed_zccache_fetch(error: &SoldrError) -> bool {
    matches!(error, SoldrError::ToolNotFound(_) | SoldrError::Network(_))
}

fn install_zccache_from_crates_io(
    paths: &SoldrPaths,
    version: &str,
    target: &TargetTriple,
) -> Result<PathBuf, SoldrError> {
    let tool_dir = paths.bin.join(format!("zccache-{version}"));
    std::fs::create_dir_all(&tool_dir)?;

    let install_root = tempfile::tempdir_in(&paths.bin)?;
    // Cargo emits a "be sure to add `<root>/bin` to your PATH" warning whenever
    // it installs into a `--root` that isn't already on PATH. The temp root
    // here is just a staging area before we copy the binaries into the
    // managed soldr cache, so the warning is misleading noise. Prepend the
    // staging bin dir to PATH for the cargo subprocess so the check passes.
    let install_bin = install_root.path().join("bin");
    let install_path_env = match std::env::var_os("PATH") {
        Some(existing) => {
            let mut dirs: Vec<PathBuf> = vec![install_bin.clone()];
            dirs.extend(std::env::split_paths(&existing));
            std::env::join_paths(dirs).map_err(|e| {
                SoldrError::Other(format!("failed to extend PATH for cargo install: {e}"))
            })?
        }
        None => install_bin.clone().into_os_string(),
    };

    for (package_name, binary_name) in MANAGED_ZCCACHE_PACKAGES {
        // Retry to tolerate a stale crates.io index cache shortly after the
        // upstream publish (e.g. a `zccache-cli vX.Y.Z` whose locked dep
        // `zccache-compiler =X.Y.Z` has not yet propagated to every mirror).
        let mut backoff = MANAGED_ZCCACHE_INSTALL_INITIAL_BACKOFF;
        let mut attempt = 1u32;
        loop {
            let mut command = std::process::Command::new("cargo");
            command
                .args([
                    "install",
                    package_name,
                    "--version",
                    version,
                    "--locked",
                    "--root",
                ])
                .arg(install_root.path())
                .args(["--bin", binary_name, "--force"])
                .env("PATH", &install_path_env)
                // Strip stale jobserver env so the nested cargo doesn't try
                // to attach to fds it cannot see (see soldr #283).
                .env_remove("MAKEFLAGS")
                .env_remove("CARGO_MAKEFLAGS");
            suppress_windows_console_window(&mut command);
            let install_status = command.status().map_err(|e| {
                SoldrError::Other(format!(
                    "failed to invoke cargo install for managed zccache package {package_name}: {e}"
                ))
            })?;

            if install_status.success() {
                break;
            }
            if attempt >= MANAGED_ZCCACHE_INSTALL_ATTEMPTS {
                return Err(SoldrError::Other(format!(
                    "cargo install {package_name} {version} failed with status {install_status}"
                )));
            }
            eprintln!(
                "soldr: cargo install {package_name} {version} failed (attempt {attempt}/{}); retrying in {:?}",
                MANAGED_ZCCACHE_INSTALL_ATTEMPTS, backoff
            );
            std::thread::sleep(backoff);
            backoff = backoff.saturating_mul(2);
            attempt += 1;
        }

        let installed_binary = install_root
            .path()
            .join("bin")
            .join(format!("{binary_name}{}", target.binary_ext()));
        if !installed_binary.exists() {
            return Err(SoldrError::Other(format!(
                "cargo install did not produce {}",
                installed_binary.display()
            )));
        }

        let cached_binary = tool_dir.join(format!("{binary_name}{}", target.binary_ext()));
        std::fs::copy(&installed_binary, &cached_binary)?;
    }

    let cached_binary = tool_dir.join(format!("zccache{}", target.binary_ext()));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for (_, binary_name) in MANAGED_ZCCACHE_PACKAGES {
            let cached_binary = tool_dir.join(format!("{binary_name}{}", target.binary_ext()));
            let mut perms = std::fs::metadata(&cached_binary)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&cached_binary, perms)?;
        }
    }

    Ok(cached_binary)
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
    fn managed_zccache_cargo_install_fallback_only_handles_unavailable_release_errors() {
        assert!(should_fallback_to_managed_zccache_cargo_install(
            &SoldrError::ToolNotFound("missing release".into())
        ));
        assert!(should_fallback_to_managed_zccache_cargo_install(
            &SoldrError::Network("github api unavailable".into())
        ));
        assert!(!should_fallback_to_managed_zccache_cargo_install(
            &SoldrError::Archive("corrupt archive".into())
        ));
        assert!(!should_fallback_to_managed_zccache_cargo_install(
            &SoldrError::Other("checksum mismatch".into())
        ));
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

    // ---------------------------------------------------------------
    // SOLDR_ZCCACHE_LOCAL_DIR override (issue: zccache #276)
    // ---------------------------------------------------------------

    fn windows_target() -> TargetTriple {
        TargetTriple {
            arch: Arch::X86_64,
            os: Os::Windows,
            env: Env::Msvc,
        }
    }

    fn linux_target() -> TargetTriple {
        TargetTriple {
            arch: Arch::X86_64,
            os: Os::Linux,
            env: Env::Gnu,
        }
    }

    fn write_fake_binary(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn local_zccache_resolves_when_all_three_binaries_present_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let local_dir = tmp.path().join("local-build");
        write_fake_binary(&local_dir, "zccache.exe", b"cli-bytes");
        write_fake_binary(&local_dir, "zccache-daemon.exe", b"daemon-bytes");
        write_fake_binary(&local_dir, "zccache-fp.exe", b"fp-bytes");

        let soldr_root = tmp.path().join("soldr");
        let paths = SoldrPaths::with_root(soldr_root);

        let result =
            resolve_local_zccache_for_target(&local_dir, &paths, &windows_target()).unwrap();
        assert!(
            result.version.starts_with("local"),
            "version: {}",
            result.version
        );
        assert!(result.binary_path.ends_with("zccache.exe"));
        assert!(result.binary_path.exists());
        let parent = result.binary_path.parent().unwrap();
        assert!(parent.join("zccache-daemon.exe").exists());
        assert!(parent.join("zccache-fp.exe").exists());
    }

    #[test]
    fn local_zccache_errors_when_daemon_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let local_dir = tmp.path().join("local-build");
        write_fake_binary(&local_dir, "zccache.exe", b"cli-bytes");
        // intentionally skip zccache-daemon.exe
        write_fake_binary(&local_dir, "zccache-fp.exe", b"fp-bytes");

        let soldr_root = tmp.path().join("soldr");
        let paths = SoldrPaths::with_root(soldr_root);

        let err = resolve_local_zccache_for_target(&local_dir, &paths, &windows_target())
            .expect_err("missing daemon should fail");
        let message = err.to_string();
        assert!(
            message.contains("zccache-daemon.exe"),
            "error must name the missing binary: {message}"
        );
        assert!(
            message.contains("SOLDR_ZCCACHE_LOCAL_DIR"),
            "error must reference the env var: {message}"
        );
    }

    #[test]
    fn local_zccache_errors_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let local_dir = tmp.path().join("does-not-exist");
        let soldr_root = tmp.path().join("soldr");
        let paths = SoldrPaths::with_root(soldr_root);

        let err = resolve_local_zccache_for_target(&local_dir, &paths, &windows_target())
            .expect_err("missing dir should fail");
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn local_zccache_copies_pdb_sidecars_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let local_dir = tmp.path().join("local-build");
        write_fake_binary(&local_dir, "zccache.exe", b"cli-bytes");
        write_fake_binary(&local_dir, "zccache-daemon.exe", b"daemon-bytes");
        write_fake_binary(&local_dir, "zccache-fp.exe", b"fp-bytes");
        // PDBs for two of three binaries.
        write_fake_binary(&local_dir, "zccache.pdb", b"cli-pdb");
        write_fake_binary(&local_dir, "zccache-daemon.pdb", b"daemon-pdb");

        let soldr_root = tmp.path().join("soldr");
        let paths = SoldrPaths::with_root(soldr_root);

        let result =
            resolve_local_zccache_for_target(&local_dir, &paths, &windows_target()).unwrap();
        let dst_dir = result.binary_path.parent().unwrap();
        assert!(
            dst_dir.join("zccache.pdb").exists(),
            "PDB next to cli should be copied"
        );
        assert!(
            dst_dir.join("zccache-daemon.pdb").exists(),
            "PDB next to daemon should be copied"
        );
        assert!(
            !dst_dir.join("zccache-fp.pdb").exists(),
            "fp.pdb wasn't in the source, so it shouldn't appear in the destination"
        );
    }

    #[test]
    fn local_zccache_succeeds_when_no_pdbs_present() {
        let tmp = tempfile::tempdir().unwrap();
        let local_dir = tmp.path().join("local-build");
        write_fake_binary(&local_dir, "zccache.exe", b"cli-bytes");
        write_fake_binary(&local_dir, "zccache-daemon.exe", b"daemon-bytes");
        write_fake_binary(&local_dir, "zccache-fp.exe", b"fp-bytes");

        let soldr_root = tmp.path().join("soldr");
        let paths = SoldrPaths::with_root(soldr_root);

        let result =
            resolve_local_zccache_for_target(&local_dir, &paths, &windows_target()).unwrap();
        assert!(result.binary_path.exists());
        // Sanity: the version label includes the content hash, so a
        // second resolution with the same bytes lands on the same dir.
        let second =
            resolve_local_zccache_for_target(&local_dir, &paths, &windows_target()).unwrap();
        assert_eq!(result.binary_path, second.binary_path);
    }

    #[test]
    fn local_zccache_unix_uses_bare_binary_names() {
        let tmp = tempfile::tempdir().unwrap();
        let local_dir = tmp.path().join("local-build");
        write_fake_binary(&local_dir, "zccache", b"cli-bytes");
        write_fake_binary(&local_dir, "zccache-daemon", b"daemon-bytes");
        write_fake_binary(&local_dir, "zccache-fp", b"fp-bytes");

        let soldr_root = tmp.path().join("soldr");
        let paths = SoldrPaths::with_root(soldr_root);

        let result = resolve_local_zccache_for_target(&local_dir, &paths, &linux_target()).unwrap();
        assert!(result.binary_path.ends_with("zccache"));
        let parent = result.binary_path.parent().unwrap();
        assert!(parent.join("zccache-daemon").exists());
        assert!(parent.join("zccache-fp").exists());
    }

    #[test]
    fn local_zccache_version_label_is_content_addressed() {
        let tmp = tempfile::tempdir().unwrap();
        let local_dir = tmp.path().join("v1");
        write_fake_binary(&local_dir, "zccache.exe", b"first-bytes");
        write_fake_binary(&local_dir, "zccache-daemon.exe", b"first-daemon");
        write_fake_binary(&local_dir, "zccache-fp.exe", b"first-fp");

        let soldr_root = tmp.path().join("soldr");
        let paths = SoldrPaths::with_root(soldr_root);
        let r1 = resolve_local_zccache_for_target(&local_dir, &paths, &windows_target()).unwrap();

        // Rewrite cli to different bytes — version label must change.
        std::fs::write(local_dir.join("zccache.exe"), b"second-bytes").unwrap();
        let r2 = resolve_local_zccache_for_target(&local_dir, &paths, &windows_target()).unwrap();
        assert_ne!(r1.version, r2.version);
        assert_ne!(r1.binary_path, r2.binary_path);
    }

    #[test]
    fn zccache_local_dir_env_var_constant_is_stable() {
        // This constant is part of the public soldr surface — any
        // rename is a breaking change for users who exported it. Keep
        // it pinned.
        assert_eq!(ZCCACHE_LOCAL_DIR_ENV_VAR, "SOLDR_ZCCACHE_LOCAL_DIR");
    }
}
