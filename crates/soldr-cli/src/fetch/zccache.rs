//! zccache binary resolution: local-build override, pinned-install,
//! managed cache snapshots, sidecar/debug-info copying, and the
//! `--zccache=system` path discovery.
//!
//! Extracted from `fetch/mod.rs` during the >1k-LOC refactor. The fetch
//! chain itself (`fetch_zccache_with_paths`, `cached_zccache_binary`,
//! crates.io fallback) lives in [`super::zccache_install`].

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths, TargetTriple};

use super::install_zccache;
use super::{
    non_empty_env_path, FetchResult, MANAGED_ZCCACHE_PACKAGES, MANAGED_ZCCACHE_VERSION,
    ZCCACHE_LOCAL_DIR_ENV_VAR,
};

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
    /// Installed via `soldr install-zccache` into the
    /// `<SoldrPaths::bin>/zccache-pinned/` directory.
    Pinned,
    /// Nothing fetched yet — managed path, no binaries on disk.
    None,
}

impl ZccacheSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ZccacheSource::Managed => "managed",
            ZccacheSource::Local => "local",
            ZccacheSource::Pinned => "pinned",
            ZccacheSource::None => "none",
        }
    }
}

/// Classify which source produced a resolved zccache binary path.
///
/// Uses the same precedence rules as `fetch_zccache_with_paths`:
/// `SOLDR_ZCCACHE_LOCAL_DIR` -> pinned-install -> managed cache. The
/// classification is path-prefix-based, so callers that already have a
/// `FetchResult` from the fetch path can ask "where did this binary
/// come from" without re-running the resolution.
///
/// Returns [`ZccacheSource::None`] when the path matches none of the
/// known source directories. Practical callers should treat that as
/// "managed" since the unlabelled fallback comes from the managed
/// download, but the explicit `None` lets diagnostics distinguish a
/// path soldr cannot recognize (a test override binary, for example)
/// from one it can.
pub fn classify_zccache_source(paths: &SoldrPaths, binary_path: &Path) -> ZccacheSource {
    let parent = match binary_path.parent() {
        Some(p) => p,
        None => return ZccacheSource::None,
    };

    // Local-build override: binaries land in a content-hashed
    // `zccache-local-<sha>` subdir under `paths.bin`. Match the dir name
    // pattern unconditionally so we still classify correctly if
    // `SOLDR_ZCCACHE_LOCAL_DIR` is no longer set in the current env.
    if let Some(name) = parent.file_name().and_then(|s| s.to_str()) {
        if name.starts_with("zccache-local-") {
            return ZccacheSource::Local;
        }
    }

    let pinned_dir = install_zccache::pinned_zccache_dir(paths);
    if parent == pinned_dir.as_path() {
        return ZccacheSource::Pinned;
    }

    let managed_dir = paths.bin.join(format!("zccache-{MANAGED_ZCCACHE_VERSION}"));
    if parent == managed_dir.as_path() {
        return ZccacheSource::Managed;
    }

    ZccacheSource::None
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

pub(crate) fn sha256_short(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let hex_digest = hex::encode(digest);
    hex_digest[..12].to_string()
}

pub(crate) fn copy_if_changed(src: &Path, dst: &Path) -> Result<(), SoldrError> {
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

pub(crate) fn copy_debug_info_sidecars(
    src_binary: &Path,
    dst_binary: &Path,
) -> Result<(), SoldrError> {
    // Windows: <binary>.pdb (sibling file).
    // Linux: <binary>.dwp (sibling file).
    // macOS: <binary>.dSYM (sibling directory).
    //
    // Rust + MSVC emits PDBs whose stem is the crate name with hyphens
    // replaced by underscores (e.g. `zccache-daemon.exe` ships
    // `zccache_daemon.pdb`). Search both naming variants so we never
    // silently miss an MSVC sidecar (issue #365).
    for sidecar_ext in ["pdb", "dwp"] {
        for src in adjacent_sidecar_candidates(src_binary, sidecar_ext) {
            if src.is_file() {
                // Mirror the source basename into the destination so
                // the copied PDB lines up with the debug record cdb
                // reads out of the binary header.
                if let Some(dst) = sidecar_dst_matching_src(&src, dst_binary, sidecar_ext) {
                    copy_if_changed(&src, &dst)?;
                }
            }
        }
    }
    for src in adjacent_sidecar_candidates(src_binary, "dSYM") {
        if src.is_dir() {
            if let Some(dst) = sidecar_dst_matching_src(&src, dst_binary, "dSYM") {
                copy_dir_recursive(&src, &dst)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn adjacent_with_extension(binary: &Path, ext: &str) -> Option<PathBuf> {
    let stem = binary.file_stem()?.to_owned();
    let parent = binary.parent()?;
    let mut name = stem;
    name.push(".");
    name.push(ext);
    Some(parent.join(name))
}

/// Sidecar lookup candidates for `binary`, trying first the literal
/// stem and then the hyphen-to-underscore variant (Rust+MSVC PDB
/// naming). Issue #365.
pub(crate) fn adjacent_sidecar_candidates(binary: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(2);
    if let Some(primary) = adjacent_with_extension(binary, ext) {
        out.push(primary);
    }
    if let (Some(stem), Some(parent)) =
        (binary.file_stem().and_then(|s| s.to_str()), binary.parent())
    {
        if stem.contains('-') {
            let underscored = stem.replace('-', "_");
            out.push(parent.join(format!("{underscored}.{ext}")));
        }
    }
    out
}

/// Pick the destination filename when copying a discovered sidecar
/// next to `dst_binary`. If `src` already uses the underscored MSVC
/// variant (e.g. `zccache_daemon.pdb`), preserve that name so the
/// debug-info record embedded in the binary continues to match.
fn sidecar_dst_matching_src(src: &Path, dst_binary: &Path, ext: &str) -> Option<PathBuf> {
    let src_name = src.file_name()?;
    let dst_parent = dst_binary.parent()?;
    let primary = adjacent_with_extension(dst_binary, ext);
    let primary_matches = primary
        .as_ref()
        .and_then(|p| p.file_name())
        .is_some_and(|name| name == src_name);
    if primary_matches {
        primary
    } else {
        Some(dst_parent.join(src_name))
    }
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

/// Snapshot of the managed (GitHub-Releases) zccache state on disk.
/// Always reports what the managed path *would* produce on next fetch,
/// even when the pinned-install path is currently winning resolution.
/// Used by `soldr doctor` to differentiate the two paths and annotate
/// "(superseded by pinned)" on the managed section when appropriate.
pub fn managed_only_zccache_summary(
    paths: &SoldrPaths,
) -> Result<ZccacheBinarySummary, SoldrError> {
    let target = TargetTriple::detect()?;
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

/// Snapshot of the pinned-install state independent of resolution
/// priority. Returns `Ok(None)` when no `source.json` exists. Used by
/// `soldr doctor` so the pinned section can render even when something
/// further up the resolution chain (e.g. `SOLDR_ZCCACHE_LOCAL_DIR`)
/// wins.
pub fn pinned_zccache_summary(
    paths: &SoldrPaths,
) -> Result<Option<ZccacheBinarySummary>, SoldrError> {
    let target = TargetTriple::detect()?;
    let Some(pinned) = install_zccache::resolve_pinned_zccache_for_target(paths, &target)? else {
        return Ok(None);
    };
    let runtime_dir = pinned.runtime_dir.clone();
    let (cli, daemon, fp) = canonical_zccache_paths(&runtime_dir, &target);
    let (debug_found, debug_expected) =
        count_debug_info_sidecars(&[cli.as_path(), daemon.as_path(), fp.as_path()]);
    Ok(Some(ZccacheBinarySummary {
        source: ZccacheSource::Pinned,
        version: pinned.version,
        symbol_path: runtime_dir.clone(),
        runtime_dir,
        source_dir: None,
        cli_path: cli.exists().then_some(cli),
        daemon_path: daemon.exists().then_some(daemon),
        fp_path: fp.exists().then_some(fp),
        debug_info_found: debug_found,
        debug_info_expected: debug_expected,
    }))
}

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

    if let Some(pinned) = install_zccache::resolve_pinned_zccache_for_target(paths, &target)? {
        if install_zccache::pinned_version_older_than_managed(&pinned.version) {
            let runtime_dir = paths.bin.join(format!("zccache-{MANAGED_ZCCACHE_VERSION}"));
            let (cli, daemon, fp) = canonical_zccache_paths(&runtime_dir, &target);
            let any_present = cli.exists() || daemon.exists() || fp.exists();
            let (debug_found, debug_expected) =
                count_debug_info_sidecars(&[cli.as_path(), daemon.as_path(), fp.as_path()]);
            return Ok(ZccacheBinarySummary {
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
            });
        }
        // `soldr install-zccache` install. Reports `pinned` so
        // doctor can surface the override (and warn the managed path
        // is superseded).
        let runtime_dir = pinned.runtime_dir.clone();
        let (cli, daemon, fp) = canonical_zccache_paths(&runtime_dir, &target);
        let (debug_found, debug_expected) =
            count_debug_info_sidecars(&[cli.as_path(), daemon.as_path(), fp.as_path()]);
        return Ok(ZccacheBinarySummary {
            source: ZccacheSource::Pinned,
            version: pinned.version,
            symbol_path: runtime_dir.clone(),
            runtime_dir,
            source_dir: None,
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

pub(crate) fn canonical_zccache_paths(
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

pub(crate) fn count_debug_info_sidecars(binaries: &[&Path]) -> (usize, usize) {
    let mut found = 0usize;
    for binary in binaries {
        if !binary.exists() {
            continue;
        }
        let mut sidecar_found = false;
        // Try `pdb` and `dwp` against both the literal binary stem and
        // the hyphen-to-underscore variant — Rust+MSVC writes PDBs as
        // `<crate_name>.pdb` even when the binary is `<crate-name>.exe`
        // (issue #365).
        'file_exts: for ext in ["pdb", "dwp"] {
            for sidecar in adjacent_sidecar_candidates(binary, ext) {
                if sidecar.is_file() {
                    sidecar_found = true;
                    break 'file_exts;
                }
            }
        }
        if !sidecar_found {
            for sidecar in adjacent_sidecar_candidates(binary, "dSYM") {
                if sidecar.is_dir() {
                    sidecar_found = true;
                    break;
                }
            }
        }
        if sidecar_found {
            found += 1;
        }
    }
    (found, binaries.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Arch, Env, Os};

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
    fn local_zccache_copies_msvc_underscored_pdb_sidecars() {
        // Rust + MSVC writes PDBs as `<crate_name>.pdb`, replacing
        // hyphens with underscores. The hyphen-only matcher in
        // `adjacent_with_extension` missed these so a fresh MSVC
        // release build of zccache reported `pdbs found: 1/3`
        // (issue #365).
        let tmp = tempfile::tempdir().unwrap();
        let local_dir = tmp.path().join("local-build");
        write_fake_binary(&local_dir, "zccache.exe", b"cli-bytes");
        write_fake_binary(&local_dir, "zccache-daemon.exe", b"daemon-bytes");
        write_fake_binary(&local_dir, "zccache-fp.exe", b"fp-bytes");
        // All three PDBs in MSVC naming.
        write_fake_binary(&local_dir, "zccache.pdb", b"cli-pdb");
        write_fake_binary(&local_dir, "zccache_daemon.pdb", b"daemon-pdb");
        write_fake_binary(&local_dir, "zccache_fp.pdb", b"fp-pdb");

        let soldr_root = tmp.path().join("soldr");
        let paths = SoldrPaths::with_root(soldr_root);

        let result =
            resolve_local_zccache_for_target(&local_dir, &paths, &windows_target()).unwrap();
        let dst_dir = result.binary_path.parent().unwrap();
        // PDBs land at the destination under their MSVC names so the
        // CodeView debug record embedded in the binary continues to
        // match (cdb compares by GUID, but only after locating the file
        // by basename — keep the same basename).
        assert!(
            dst_dir.join("zccache.pdb").exists(),
            "cli PDB should be copied: {dst_dir:?}",
        );
        assert!(
            dst_dir.join("zccache_daemon.pdb").exists(),
            "underscored daemon PDB should be copied: {dst_dir:?}",
        );
        assert!(
            dst_dir.join("zccache_fp.pdb").exists(),
            "underscored fp PDB should be copied: {dst_dir:?}",
        );

        // Doctor's debug-info counter must agree — issue #365 acceptance
        // criterion says a fresh MSVC build should report `3/3`.
        let (found, expected) = count_debug_info_sidecars(&[
            dst_dir.join("zccache.exe").as_path(),
            dst_dir.join("zccache-daemon.exe").as_path(),
            dst_dir.join("zccache-fp.exe").as_path(),
        ]);
        assert_eq!(expected, 3);
        assert_eq!(found, 3, "all three PDBs must be discovered");
    }

    #[test]
    fn count_debug_info_sidecars_finds_underscored_pdb_variants() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("zccache.exe"), b"cli").unwrap();
        std::fs::write(dir.join("zccache-daemon.exe"), b"daemon").unwrap();
        std::fs::write(dir.join("zccache-fp.exe"), b"fp").unwrap();
        std::fs::write(dir.join("zccache.pdb"), b"cli-pdb").unwrap();
        std::fs::write(dir.join("zccache_daemon.pdb"), b"daemon-pdb").unwrap();
        std::fs::write(dir.join("zccache_fp.pdb"), b"fp-pdb").unwrap();

        let (found, expected) = count_debug_info_sidecars(&[
            dir.join("zccache.exe").as_path(),
            dir.join("zccache-daemon.exe").as_path(),
            dir.join("zccache-fp.exe").as_path(),
        ]);
        assert_eq!(expected, 3);
        assert_eq!(found, 3, "all three PDBs should be discovered");
    }

    #[test]
    fn count_debug_info_sidecars_still_finds_hyphenated_pdbs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("zccache-daemon.exe"), b"daemon").unwrap();
        std::fs::write(dir.join("zccache-daemon.pdb"), b"daemon-pdb").unwrap();

        let (found, expected) =
            count_debug_info_sidecars(&[dir.join("zccache-daemon.exe").as_path()]);
        assert_eq!(expected, 1);
        assert_eq!(found, 1);
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
