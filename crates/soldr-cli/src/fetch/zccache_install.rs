//! zccache fetch chain (GitHub Releases → crates.io fallback) and
//! `--zccache=system` path discovery. Resolution helpers and binary
//! summaries live in [`super::zccache`].

use std::path::PathBuf;

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths, TargetTriple};

use super::github::RepoInfo;
use super::zccache::resolve_local_zccache_for_target;
use super::{
    check_cache, fetch_repo_binary_with_paths, install_zccache, non_empty_env_path, FetchResult,
    VersionSpec, MANAGED_ZCCACHE_INSTALL_ATTEMPTS, MANAGED_ZCCACHE_INSTALL_INITIAL_BACKOFF,
    MANAGED_ZCCACHE_PACKAGES, MANAGED_ZCCACHE_VERSION, ZCCACHE_LOCAL_DIR_ENV_VAR,
};

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

    // Pinned install (`soldr install-zccache <SOURCE>`). Sits
    // between the env-var override and the managed download so users
    // who pinned once never go back to the GitHub-Releases path.
    //
    // We deliberately do NOT print a "using pinned zccache from ..."
    // line here: the cargo front door now emits a source-aware
    // `soldr: zccache source: pinned|local|managed (...)` line via
    // `classify_zccache_source` so users see exactly which binary won
    // resolution. See issue #420 — the old per-branch eprintln paired
    // with prepare_zccache_build's hard-coded "soldr: using managed
    // zccache 1.8.1" was the smoking gun that fooled the perf-cluster
    // debugging session.
    if let Some(pinned) = install_zccache::resolve_pinned_zccache_for_target(paths, &target)? {
        return Ok(FetchResult {
            binary_path: pinned.binary_path,
            version: pinned.version,
            cached: true,
        });
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
    // `fetch_repo_binary_with_paths` retries transient fetch errors
    // internally (issue: flaky GitHub-Releases lookups during propagation
    // windows surfaced as macOS CI flake on PR #431). Anything that
    // survives those retries is either a hard 404 or a non-transient
    // network class — both of which the cargo install fallback below can
    // handle.
    let repo = managed_zccache_repo();
    let release_outcome = fetch_repo_binary_with_paths(
        "zccache",
        &binary_names,
        &repo,
        &release_version,
        None,
        paths,
    )
    .await;

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

    if let Some(pinned) = install_zccache::resolve_pinned_zccache_for_target(paths, &target)? {
        return Ok(Some(FetchResult {
            binary_path: pinned.binary_path,
            version: pinned.version,
            cached: true,
        }));
    }

    check_cache(
        paths,
        "zccache",
        MANAGED_ZCCACHE_VERSION,
        &["zccache", "zccache-daemon", "zccache-fp"],
        &target,
    )
}

/// Locate `zccache` on the system `PATH` and use it directly instead
/// of the managed-fetch path. Requires the daemon/fingerprint sibling
/// binaries to live next to `zccache` in the same directory, which is
/// how every supported zccache installer lays them out.
///
/// Driven by the top-level `--zccache=system` CLI flag.
pub fn resolve_system_zccache(_paths: &SoldrPaths) -> Result<FetchResult, SoldrError> {
    let target = TargetTriple::detect()?;
    resolve_system_zccache_for_target(&target)
}

pub(crate) fn resolve_system_zccache_for_target(
    target: &TargetTriple,
) -> Result<FetchResult, SoldrError> {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    resolve_system_zccache_with_path_dirs(target, &path_dirs)
}

pub(crate) fn resolve_system_zccache_with_path_dirs(
    target: &TargetTriple,
    path_dirs: &[PathBuf],
) -> Result<FetchResult, SoldrError> {
    let binary_ext = target.binary_ext();
    let zccache_name = format!("zccache{binary_ext}");
    let zccache_path = find_in_dirs(path_dirs, &zccache_name).ok_or_else(|| {
        SoldrError::Other(format!(
            "`--zccache=system` requested but `{zccache_name}` was not found on PATH; install zccache or drop the flag to use the managed download"
        ))
    })?;

    let dir = zccache_path.parent().ok_or_else(|| {
        SoldrError::Other(format!(
            "system zccache at {} has no parent directory",
            zccache_path.display()
        ))
    })?;

    for sibling in ["zccache-daemon", "zccache-fp"] {
        let sibling_path = dir.join(format!("{sibling}{binary_ext}"));
        if !sibling_path.is_file() {
            return Err(SoldrError::Other(format!(
                "`--zccache=system`: expected {} next to {}",
                sibling_path.display(),
                zccache_path.display()
            )));
        }
    }

    eprintln!("soldr: using system zccache at {}", zccache_path.display());
    Ok(FetchResult {
        binary_path: zccache_path,
        version: "system".to_string(),
        cached: false,
    })
}

pub(crate) fn find_in_dirs(dirs: &[PathBuf], file_name: &str) -> Option<PathBuf> {
    for dir in dirs {
        let candidate = dir.join(file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn managed_zccache_repo() -> RepoInfo {
    RepoInfo {
        owner: "zackees".to_string(),
        repo: "zccache".to_string(),
    }
}

pub(crate) fn should_fallback_to_managed_zccache_cargo_install(error: &SoldrError) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Arch, Env, Os};
    use std::path::{Path, PathBuf};

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
    fn system_zccache_resolves_when_all_three_binaries_on_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        write_fake_binary(&bin_dir, "zccache.exe", b"cli-bytes");
        write_fake_binary(&bin_dir, "zccache-daemon.exe", b"daemon-bytes");
        write_fake_binary(&bin_dir, "zccache-fp.exe", b"fp-bytes");

        let dirs = vec![bin_dir.clone()];
        let result = resolve_system_zccache_with_path_dirs(&windows_target(), &dirs).unwrap();
        assert_eq!(result.version, "system");
        assert_eq!(result.binary_path, bin_dir.join("zccache.exe"));
    }

    #[test]
    fn system_zccache_errors_when_zccache_not_on_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        let dirs = vec![bin_dir];
        let err = resolve_system_zccache_with_path_dirs(&windows_target(), &dirs)
            .expect_err("missing zccache on PATH must fail");
        let message = err.to_string();
        assert!(
            message.contains("--zccache=system"),
            "error must reference the flag: {message}"
        );
        assert!(
            message.contains("PATH"),
            "error must reference PATH: {message}"
        );
    }

    #[test]
    fn system_zccache_errors_when_daemon_sibling_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        write_fake_binary(&bin_dir, "zccache.exe", b"cli-bytes");
        write_fake_binary(&bin_dir, "zccache-fp.exe", b"fp-bytes");

        let dirs = vec![bin_dir];
        let err = resolve_system_zccache_with_path_dirs(&windows_target(), &dirs)
            .expect_err("missing daemon sibling must fail");
        let message = err.to_string();
        assert!(
            message.contains("zccache-daemon.exe"),
            "error must name the missing sibling: {message}"
        );
    }

    #[test]
    fn system_zccache_unix_uses_bare_binary_names() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        write_fake_binary(&bin_dir, "zccache", b"cli-bytes");
        write_fake_binary(&bin_dir, "zccache-daemon", b"daemon-bytes");
        write_fake_binary(&bin_dir, "zccache-fp", b"fp-bytes");

        let dirs = vec![bin_dir.clone()];
        let result = resolve_system_zccache_with_path_dirs(&linux_target(), &dirs).unwrap();
        assert!(result.binary_path.ends_with("zccache"));
    }
}
