//! Test-only helpers that must not enable zccache's standalone CLI features.

use std::path::PathBuf;

/// Return Cargo's configured compiler, falling back to PATH for direct tests.
pub(crate) fn rustc_from_env_or_path() -> PathBuf {
    std::env::var_os("RUSTC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rustc"))
}
