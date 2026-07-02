//! Manual uv-provisioned maturin environment — soldr#1264 follow-on.
//!
//! The primary maturin acquisition is the pinned prebuilt binary from
//! PyO3/maturin GitHub Releases (`fetch_tool("maturin", pinned)`).
//! This module is the guarantee behind "maturin is always invocable":
//! when the binary fetch misses (rate limit, missing asset for a
//! platform, upstream release shape change), soldr provisions maturin
//! into a manually-managed isolated environment using the managed
//! `uv` from the soldr-toolchain archive:
//!
//! ```text
//! uv venv --python 3.12 ~/.soldr/bin/maturin-uv-<ver>/
//! uv pip install --python <env python> maturin==<ver>
//! ```
//!
//! Deliberately hand-rolled rather than depending on the `uv-iso-env`
//! PyPI package: soldr's build backend carries zero third-party Python
//! deps, and the two uv invocations above are the entire surface that
//! package would have wrapped.
//!
//! uv state is kept under the soldr root (`UV_CACHE_DIR`,
//! `UV_PYTHON_INSTALL_DIR`) so nothing leaks into the user's uv dirs
//! and `soldr clean`-style wipes take it along.
//!
//! ## Env-var surface
//!
//! `SOLDR_MATURIN_PROVISIONER` — `auto` (default: prebuilt binary,
//! uv-env fallback), `binary` (prebuilt only, fail hard on miss),
//! `uv` (skip the binary fetch, go straight to the uv env).

use std::path::{Path, PathBuf};

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};

/// Selects how `soldr maturin` acquires the maturin executable.
pub const MATURIN_PROVISIONER_ENV_VAR: &str = "SOLDR_MATURIN_PROVISIONER";

/// Python version the isolated maturin env is created with. uv
/// downloads its managed CPython on demand when the host has none —
/// pinned so the env is reproducible across machines.
pub const MATURIN_ENV_PYTHON: &str = "3.12";

/// Sentinel filename marking a fully-provisioned env. Written last;
/// its absence means a previous attempt died mid-way and the dir is
/// rebuilt from scratch.
const COMPLETE_SENTINEL: &str = ".soldr-complete";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaturinProvisioner {
    /// Prebuilt binary first, uv env as fallback (default).
    Auto,
    /// Prebuilt binary only — a fetch miss is a hard error.
    Binary,
    /// Straight to the uv-provisioned env.
    Uv,
}

impl MaturinProvisioner {
    /// Parse from the env-var value. Unknown / empty values fall back
    /// to `Auto` — diagnostic env vars must never wedge the build.
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(v) if v.eq_ignore_ascii_case("binary") => Self::Binary,
            Some(v) if v.eq_ignore_ascii_case("uv") => Self::Uv,
            _ => Self::Auto,
        }
    }

    pub fn from_env() -> Self {
        let raw = std::env::var(MATURIN_PROVISIONER_ENV_VAR).ok();
        Self::from_env_value(raw.as_deref())
    }
}

/// Directory of the isolated maturin env for `version`.
pub fn env_dir_for(paths: &SoldrPaths, version: &str) -> PathBuf {
    paths.bin.join(format!("maturin-uv-{version}"))
}

/// Path of the maturin executable inside an isolated env dir.
pub fn maturin_exe_in_env(env_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        env_dir.join("Scripts").join("maturin.exe")
    } else {
        env_dir.join("bin").join("maturin")
    }
}

/// A provisioned env counts as complete only when BOTH the maturin
/// executable and the sentinel exist — the sentinel is written last,
/// so its absence means a prior attempt died mid-way and the dir must
/// be rebuilt rather than served.
pub(crate) fn env_is_complete(env_dir: &Path) -> bool {
    maturin_exe_in_env(env_dir).is_file() && env_dir.join(COMPLETE_SENTINEL).is_file()
}

fn python_exe_in_env(env_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        env_dir.join("Scripts").join("python.exe")
    } else {
        env_dir.join("bin").join("python")
    }
}

/// Provision maturin `version` into an isolated uv-managed env and
/// return the maturin executable path. Idempotent: a completed env is
/// a couple of stat calls; a half-built one is wiped and rebuilt.
pub async fn provision_maturin_via_uv(
    paths: &SoldrPaths,
    version: &str,
) -> Result<PathBuf, SoldrError> {
    let env_dir = env_dir_for(paths, version);
    let maturin = maturin_exe_in_env(&env_dir);
    if env_is_complete(&env_dir) {
        return Ok(maturin);
    }

    let host = super::cmake_tools::current_host_triple();
    let uv_root = super::uv_tool::ensure_uv_bundle(paths, host).await?;
    let uv = super::uv_tool::uv_exe(&uv_root);
    if !uv.is_file() {
        return Err(SoldrError::Other(format!(
            "managed uv bundle at {} has no {}",
            uv_root.display(),
            uv.display()
        )));
    }

    // Half-built env from a previous attempt → start clean.
    if env_dir.exists() {
        let _ = std::fs::remove_dir_all(&env_dir);
    }

    eprintln!(
        "soldr: provisioning maturin {version} via managed uv \
         ({MATURIN_ENV_PYTHON} isolated env)..."
    );

    run_uv(
        &uv,
        paths,
        &[
            "venv".as_ref(),
            "--python".as_ref(),
            MATURIN_ENV_PYTHON.as_ref(),
            env_dir.as_os_str(),
        ],
        "uv venv",
    )?;

    let env_python = python_exe_in_env(&env_dir);
    let spec = format!("maturin=={version}");
    run_uv(
        &uv,
        paths,
        &[
            "pip".as_ref(),
            "install".as_ref(),
            "--python".as_ref(),
            env_python.as_os_str(),
            spec.as_str().as_ref(),
        ],
        "uv pip install maturin",
    )?;

    if !maturin.is_file() {
        return Err(SoldrError::Other(format!(
            "uv reported success but {} does not exist",
            maturin.display()
        )));
    }
    std::fs::write(env_dir.join(COMPLETE_SENTINEL), b"ok\n")?;
    Ok(maturin)
}

/// Run one uv command with soldr-scoped uv state dirs. Inherits
/// stdout/stderr so download progress is visible to the user.
fn run_uv(
    uv: &Path,
    paths: &SoldrPaths,
    args: &[&std::ffi::OsStr],
    label: &str,
) -> Result<(), SoldrError> {
    let mut command = std::process::Command::new(uv);
    command.args(args);
    command.env("UV_CACHE_DIR", paths.root.join("uv-cache"));
    command.env("UV_PYTHON_INSTALL_DIR", paths.root.join("uv-python"));
    suppress_windows_console_window(&mut command);
    let status = command.status().map_err(|e| {
        SoldrError::Other(format!("{label}: failed to spawn {}: {e}", uv.display()))
    })?;
    if !status.success() {
        return Err(SoldrError::Other(format!(
            "{label} exited with {status} (uv: {})",
            uv.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(provisioner_parse_defaults_to_auto, {
        assert_eq!(
            MaturinProvisioner::from_env_value(None),
            MaturinProvisioner::Auto
        );
        assert_eq!(
            MaturinProvisioner::from_env_value(Some("")),
            MaturinProvisioner::Auto
        );
        assert_eq!(
            MaturinProvisioner::from_env_value(Some("garbage")),
            MaturinProvisioner::Auto,
            "unknown values must not wedge the build"
        );
        assert_eq!(
            MaturinProvisioner::from_env_value(Some("BINARY")),
            MaturinProvisioner::Binary
        );
        assert_eq!(
            MaturinProvisioner::from_env_value(Some(" uv ")),
            MaturinProvisioner::Uv
        );
    });

    crate::timed_test!(env_layout_is_versioned_and_platform_correct, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let dir = env_dir_for(&paths, "1.14.1");
        assert!(dir.ends_with("maturin-uv-1.14.1"));
        let exe = maturin_exe_in_env(&dir);
        if cfg!(windows) {
            assert!(exe.ends_with("Scripts/maturin.exe") || exe.ends_with("Scripts\\maturin.exe"));
        } else {
            assert!(exe.ends_with("bin/maturin"));
        }
    });

    crate::timed_test!(completed_env_short_circuits_without_uv, {
        // A maturin exe + sentinel must satisfy the provisioner with
        // zero network / uv-bundle work — proven by pointing at a
        // synthetic root where no uv bundle could possibly exist.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let env_dir = env_dir_for(&paths, "9.9.9");
        let exe = maturin_exe_in_env(&env_dir);
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"stub").unwrap();
        std::fs::write(env_dir.join(COMPLETE_SENTINEL), b"ok\n").unwrap();

        let got = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(provision_maturin_via_uv(&paths, "9.9.9"))
            .expect("completed env must short-circuit");
        assert_eq!(got, exe);
    });

    crate::timed_test!(missing_sentinel_means_incomplete, {
        // Exe without sentinel = half-built env → NOT complete, must
        // be rebuilt rather than served. Sentinel without exe likewise.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let env_dir = tmp.path().join("maturin-uv-9.9.9");
        let exe = maturin_exe_in_env(&env_dir);
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"stub").unwrap();
        assert!(!env_is_complete(&env_dir), "no sentinel → incomplete");

        std::fs::write(env_dir.join(COMPLETE_SENTINEL), b"ok\n").unwrap();
        assert!(env_is_complete(&env_dir));

        std::fs::remove_file(&exe).unwrap();
        assert!(!env_is_complete(&env_dir), "no exe → incomplete");
    });
}
