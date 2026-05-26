//! Shared foundational types for soldr.
//!
//! The `core` module owns target-triple resolution, `~/.soldr/`
//! layout (`SoldrPaths` / `SoldrConfig`), `SoldrError`, and the
//! runtime toolchain-discovery helpers. It is split across
//! sub-modules but the external API (`crate::core::*` /
//! `soldr_cli::core::*`) is preserved via re-exports below — every
//! identifier consumers reach for must show up in this `mod.rs`.

use std::ffi::OsStr;
use std::path::PathBuf;

use thiserror::Error;

mod paths;
mod target_triple;
mod toolchain_manifest;
mod toolchain_resolve;

pub use paths::{
    resolve_cargo_home, resolve_rustup_home, AutoGcConfig, GcConfig, SoldrConfig, SoldrPaths,
    SOLDR_CACHE_DIR_ENV_VAR,
};
pub use target_triple::{Arch, Env, Os, TargetTriple};
pub use toolchain_manifest::{
    read_rust_toolchain_manifest, PluginSpec, RustToolchainManifest, SoldrManifestSection,
};
pub use toolchain_resolve::{
    apply_implicit_toolchain_homes, probe_toolchain_binary, suppress_windows_console_window,
};

pub const CARGO_HOME_ENV_VAR: &str = "CARGO_HOME";
pub const RUSTUP_HOME_ENV_VAR: &str = "RUSTUP_HOME";
pub(crate) const RUSTUP_TOOLCHAIN_ENV_VAR: &str = "RUSTUP_TOOLCHAIN";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum SoldrError {
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
    #[error("no home directory found")]
    NoHomeDir,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("network error: {0}")]
    Network(String),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("archive error: {0}")]
    Archive(String),
    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------------
// Home-dir resolution + small env helpers
// ---------------------------------------------------------------------------

/// Public accessor for the soldr home directory used by the `gc`
/// allowlist default (`~/dev`). Returns `Err` when no home dir can be
/// resolved.
pub fn user_home_dir() -> Result<PathBuf, SoldrError> {
    home_dir()
}

/// Expand `~` and `~/...` strings to absolute paths under the user's
/// home directory. Other inputs pass through unchanged.
pub fn expand_user_home(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~") {
        if let Ok(home) = home_dir() {
            let trimmed = rest.trim_start_matches(['/', '\\']);
            if trimmed.is_empty() {
                return home;
            }
            return home.join(trimmed);
        }
    }
    PathBuf::from(input)
}

pub(crate) fn soldr_root_from_env_var(
    value: Option<&OsStr>,
) -> Option<Result<PathBuf, SoldrError>> {
    non_empty_env_path(value).map(Ok)
}

pub(crate) fn non_empty_env_path(value: Option<&OsStr>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

pub(crate) fn home_dir() -> Result<PathBuf, SoldrError> {
    #[cfg(windows)]
    {
        if let Ok(p) = std::env::var("USERPROFILE") {
            return Ok(PathBuf::from(p));
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(p) = std::env::var("HOME") {
            return Ok(PathBuf::from(p));
        }
    }
    Err(SoldrError::NoHomeDir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
