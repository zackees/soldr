//! Transient child-process shims (issue #493).
//!
//! When a user runs `soldr <external-tool> ...` (e.g. `soldr maturin
//! build`), the external tool runs as itself but any `cargo` / `rustc`
//! / etc it spawns internally would normally bypass soldr because PATH
//! still resolves to the unwrapped system binaries.
//!
//! This module creates a transient directory of thin shims for the
//! Rust toolchain binaries soldr already wraps (`cargo`, `rustc`,
//! `rustdoc`, `rustfmt`, `clippy-driver`). Each shim re-invokes the
//! parent soldr binary with the matching verb, so nested toolchain
//! calls route back through soldr (and therefore zccache, the managed
//! toolchain home, etc) without the user having to mess with PATH.
//!
//! A recursion guard env var (`SOLDR_CHILD_SHIMS_ACTIVE`) is set in the
//! child environment so a nested `soldr <external-tool>` invocation
//! sees the sentinel and does NOT re-install another shim layer.

use crate::core::SoldrError;
use std::path::{Path, PathBuf};

/// Sentinel that signals "you were invoked under a soldr shim dir;
/// do NOT install another shim layer for your own children." Read by
/// `should_install_shims` and set by `apply_to_command`.
pub(crate) const SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR: &str = "SOLDR_CHILD_SHIMS_ACTIVE";

/// Opt-out toggle. When set to a truthy value, `should_install_shims`
/// returns false and the external tool runs without a shim layer.
pub(crate) const SOLDR_DISABLE_CHILD_SHIMS_ENV_VAR: &str = "SOLDR_DISABLE_CHILD_SHIMS";

/// Names installed into the shim dir. Mirrors the `Commands` variants
/// that route to `toolchain::run_toolchain_passthrough` so we only
/// proxy tools soldr already knows how to wrap.
const SHIMMED_TOOLS: &[&str] = &["cargo", "rustc", "rustdoc", "rustfmt", "clippy-driver"];

/// Drop-on-exit guard that removes the shim directory best-effort.
/// Holding the guard alive across the child's run is the caller's
/// responsibility.
pub(crate) struct ShimDirGuard {
    pub(crate) path: PathBuf,
}

impl Drop for ShimDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Decide whether to install shims for the next child process. Returns
/// `false` when the recursion guard is tripped or when the user has
/// opted out via `SOLDR_DISABLE_CHILD_SHIMS`.
pub(crate) fn should_install_shims() -> bool {
    if std::env::var_os(SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR).is_some() {
        return false;
    }
    if let Some(raw) = std::env::var_os(SOLDR_DISABLE_CHILD_SHIMS_ENV_VAR) {
        let lowered = raw.to_string_lossy().trim().to_ascii_lowercase();
        if !matches!(lowered.as_str(), "" | "0" | "false" | "no" | "off") {
            return false;
        }
    }
    true
}

/// Build a fresh shim dir under the system tempdir and populate it
/// with one shim per `SHIMMED_TOOLS` entry. Each shim execs the parent
/// soldr binary with the corresponding verb.
pub(crate) fn build_shim_dir() -> Result<ShimDirGuard, SoldrError> {
    let soldr_bin = crate::current_soldr_binary()?;
    let dir = tempfile::Builder::new()
        .prefix("soldr-shims-")
        .tempdir()
        .map_err(SoldrError::Io)?;
    let dir_path = dir.path().to_path_buf();
    // Defuse the tempdir auto-cleanup; ShimDirGuard owns removal so
    // the lifetime matches the child process duration regardless of
    // panic / early return paths.
    let _ = dir.keep();
    for tool in SHIMMED_TOOLS {
        write_shim(&dir_path, tool, &soldr_bin)?;
    }
    Ok(ShimDirGuard { path: dir_path })
}

#[cfg(windows)]
fn write_shim(dir: &Path, tool: &str, soldr_bin: &Path) -> Result<(), SoldrError> {
    // .cmd is the simplest cross-tool extension Windows resolves
    // automatically from PATH. Quoting the soldr path handles spaces.
    let path = dir.join(format!("{tool}.cmd"));
    let body = format!("@echo off\r\n\"{}\" {} %*\r\n", soldr_bin.display(), tool);
    std::fs::write(&path, body).map_err(SoldrError::Io)
}

#[cfg(unix)]
fn write_shim(dir: &Path, tool: &str, soldr_bin: &Path) -> Result<(), SoldrError> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(tool);
    let body = format!(
        "#!/bin/sh\nexec \"{}\" {} \"$@\"\n",
        soldr_bin.display(),
        tool
    );
    std::fs::write(&path, body).map_err(SoldrError::Io)?;
    let mut perms = std::fs::metadata(&path)
        .map_err(SoldrError::Io)?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).map_err(SoldrError::Io)?;
    Ok(())
}

/// Apply the shim dir to `command`'s environment so the child sees it
/// at the front of PATH and inherits the recursion sentinel.
pub(crate) fn apply_to_command(command: &mut std::process::Command, shim_dir: &Path) {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    // Build "shim_dir<sep>existing" without copying the OsString — we
    // need OS-specific separators (';' on Windows, ':' elsewhere). The
    // canonical way is `env::join_paths`, but that requires a Vec of
    // paths; doing it by hand here is simpler and equivalent.
    let mut new_path = std::ffi::OsString::new();
    new_path.push(shim_dir.as_os_str());
    if !existing.is_empty() {
        #[cfg(windows)]
        new_path.push(";");
        #[cfg(not(windows))]
        new_path.push(":");
        new_path.push(&existing);
    }
    command.env("PATH", new_path);
    command.env(SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR, "1");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn shim_dir_contains_every_shimmed_tool() {
        let guard = build_shim_dir().expect("build_shim_dir");
        for tool in SHIMMED_TOOLS {
            #[cfg(windows)]
            let expected = guard.path.join(format!("{tool}.cmd"));
            #[cfg(not(windows))]
            let expected = guard.path.join(tool);
            assert!(expected.is_file(), "missing shim at {}", expected.display());
        }
    }

    #[test]
    fn apply_to_command_sets_recursion_sentinel_and_prepends_path() {
        let guard = build_shim_dir().expect("build_shim_dir");
        let mut cmd = std::process::Command::new("does-not-matter");
        apply_to_command(&mut cmd, &guard.path);
        let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();
        let sentinel_set = envs
            .get(OsStr::new(SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR))
            .copied()
            .flatten();
        assert_eq!(sentinel_set, Some(OsStr::new("1")));
        let path_value = envs.get(OsStr::new("PATH")).copied().flatten();
        let path_str = path_value
            .expect("PATH must be set")
            .to_string_lossy()
            .to_string();
        assert!(
            path_str.starts_with(&guard.path.to_string_lossy().to_string()),
            "PATH must lead with the shim dir: {path_str}"
        );
    }

    #[test]
    fn should_install_shims_respects_recursion_sentinel() {
        // Test seam: this test must not pollute the parent env. We set
        // the var, observe the guard, then unset.
        std::env::set_var(SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR, "1");
        let active = should_install_shims();
        std::env::remove_var(SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR);
        assert!(!active);
    }

    #[test]
    fn should_install_shims_respects_opt_out() {
        std::env::set_var(SOLDR_DISABLE_CHILD_SHIMS_ENV_VAR, "1");
        let active = should_install_shims();
        std::env::remove_var(SOLDR_DISABLE_CHILD_SHIMS_ENV_VAR);
        assert!(!active);
    }
}
