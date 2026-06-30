//! Python sysroot fetcher — Phase A of soldr#997 (closes parts of #931,
//! #932, #933).
//!
//! On Linux x86 docker → any-target-triple cross-compile, building any
//! PyO3-using crate needs:
//!   * Windows: `python3.lib` + `python<ver>.lib` import libs + headers
//!   * macOS:  `libpython3.<ver>.dylib` + headers
//!   * Linux:  `libpython3.<ver>.so` + headers
//!
//! python-build-standalone (astral-sh) publishes per-target sysroot
//! tarballs that the soldr-toolchain `recipes/python-<triple-slug>/`
//! Conan recipes extract + repackage as catalogue rows under
//! `python/<py-version>/<plat-arch-slug>/sysroot.tar.zst`.
//!
//! This module is the **soldr-side consumer** of those catalogue rows.
//! It:
//!   * Maps a Rust target triple → the catalogue slug (`windows-x86_64-msvc`,
//!     `darwin-x86_64`, `linux-aarch64-musl`, …)
//!   * Constructs the expected `media.githubusercontent.com/media/.../sysroot.tar.zst`
//!     URL on the soldr-toolchain `assets` branch
//!   * Stamps + caches under `~/.soldr/sdk/<triple>/python<ver>/`
//!   * Surfaces the resulting paths so the cargo front door can inject
//!     `PYO3_CROSS_LIB_DIR=<sysroot>/lib` and
//!     `PYO3_CROSS_PYTHON_VERSION=<py-version>` on the child cargo
//!     (#939 stage 2 will call into this from `pyo3_detect`).
//!
//! ### Catalogue ingest status
//!
//! As of this PR, the soldr-toolchain `recipes/python-*` recipes are
//! all merged but the `forge-conan.yml` dispatches that ingest the
//! resulting tarballs into the catalogue's `assets` branch are still
//! running. Until those land + a follow-up PR fills in the per-triple
//! sha256 constants below, `ensure_python_sysroot` returns
//! `SoldrError::Other` with a clear "catalogue not yet populated" hint.
//! The lookup tables + URL builders are exercised by the unit tests in
//! this module so the slug shape is locked.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

/// Default Python version the soldr cross-compile fetcher targets when
/// the caller doesn't override. Picked to match the most-used PyO3 +
/// maturin combination at time of writing. Bump in lockstep with the
/// soldr-toolchain `_python_pbs.PBS_TAGS` table.
pub const DEFAULT_PYTHON_VERSION: &str = "3.13.0";

/// Env-var override for the Python version targeted by cross-compile.
/// When unset → [`DEFAULT_PYTHON_VERSION`]. When set to a value in
/// [`SUPPORTED_PYTHON_VERSIONS`] → that. When set to anything else →
/// warning printed, falls back to default. Aligns with how
/// `SOLDR_APPLE_SDK_VERSION` works for the Apple SDK.
pub const PYTHON_VERSION_ENV_VAR: &str = "SOLDR_PYTHON_VERSION";

/// Versions soldr knows how to fetch from the catalogue. Must match
/// `PBS_TAGS` in `soldr-toolchain/recipes/_python_pbs.py` — drift means
/// a soldr fetch lands on a 404.
pub const SUPPORTED_PYTHON_VERSIONS: &[&str] = &["3.13.0", "3.12.7", "3.11.10", "3.10.15"];

/// The 8 canonical target triples soldr ships sysroot recipes for.
/// First element = Rust target triple (the soldr-side name). Second
/// element = catalogue slug (`recipes/python-<slug>/`).
pub const PYTHON_SYSROOT_TARGETS: &[(&str, &str)] = &[
    ("x86_64-pc-windows-msvc", "windows-x64"),
    ("aarch64-pc-windows-msvc", "windows-arm64"),
    ("x86_64-apple-darwin", "darwin-x64"),
    ("aarch64-apple-darwin", "darwin-arm64"),
    ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
    ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
    ("x86_64-unknown-linux-musl", "linux-x64-musl"),
    ("aarch64-unknown-linux-musl", "linux-arm64-musl"),
];

/// Look up the catalogue slug for a Rust target triple.
pub fn catalogue_slug_for(triple: &str) -> Option<&'static str> {
    PYTHON_SYSROOT_TARGETS
        .iter()
        .find(|(rust_triple, _)| *rust_triple == triple)
        .map(|(_, slug)| *slug)
}

/// Construct the expected `assets`-branch URL for a Python sysroot
/// asset. The catalogue producer pipeline (forge-conan.yml → ingest)
/// publishes under this layout:
///
///     python/<py-version>/<slug>/bundle.tar.zst
///
/// `media.githubusercontent.com/media/` is used (not `raw`) so LFS-
/// tracked blobs follow their pointer files to the actual bytes —
/// matching the apple-sdk fetcher's pattern.
pub fn asset_url_for(py_version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("python", py_version, slug)
}

/// Resolve the Python version soldr should fetch. Precedence:
/// [`PYTHON_VERSION_ENV_VAR`] > [`DEFAULT_PYTHON_VERSION`].
pub fn resolve_python_version() -> String {
    if let Ok(value) = std::env::var(PYTHON_VERSION_ENV_VAR) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            if SUPPORTED_PYTHON_VERSIONS.contains(&trimmed) {
                return trimmed.to_string();
            }
            eprintln!(
                "soldr: warning: {PYTHON_VERSION_ENV_VAR}={trimmed:?} not in supported set \
                 {SUPPORTED_PYTHON_VERSIONS:?}; falling back to {DEFAULT_PYTHON_VERSION}."
            );
        }
    }
    DEFAULT_PYTHON_VERSION.to_string()
}

/// Ensure a Python sysroot is materialized for the given target triple.
///
/// Returns the sysroot directory containing `lib/` + `include/`. The
/// caller (cargo front door, #939 stage 2) sets `PYO3_CROSS_LIB_DIR` to
/// this dir and `PYO3_CROSS_PYTHON_VERSION` to the version it requested.
///
/// **Status (as of this PR):** the catalogue rows that back this
/// fetcher are still being ingested by the soldr-toolchain pipeline.
/// Until the per-triple sha256 constants in this module are populated
/// (follow-up PR), this function returns a clear `SoldrError::Other`
/// pointing at soldr#997 so the failure mode is debuggable.
pub async fn ensure_python_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no python sysroot recipe for target {target_triple}; \
             supported: {:?}",
            PYTHON_SYSROOT_TARGETS
                .iter()
                .map(|(t, _)| *t)
                .collect::<Vec<_>>()
        ))
    })?;
    let version = resolve_python_version();
    super::syslib_common::ensure_syslib_bundle(paths, "python", &version, slug).await
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(catalogue_covers_all_canonical_targets, {
        for (triple, _slug) in PYTHON_SYSROOT_TARGETS {
            assert!(
                crate::core::canonical_targets::is_canonical(triple),
                "{triple} not in canonical 8-target list"
            );
        }
        for canonical in crate::core::canonical_targets::canonical_targets() {
            assert!(
                catalogue_slug_for(canonical).is_some(),
                "canonical target {canonical} has no python sysroot row"
            );
        }
    });

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let u = asset_url_for("3.13.0", "windows-x64");
        assert!(u.starts_with("https://media.githubusercontent.com/media/"));
        assert!(u.contains("/zackees/soldr-toolchain/assets/"));
        assert!(u.contains("/python/3.13.0/windows-x64/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(catalogue_slug_for_known_triples, {
        assert_eq!(
            catalogue_slug_for("x86_64-pc-windows-msvc"),
            Some("windows-x64")
        );
        assert_eq!(
            catalogue_slug_for("aarch64-pc-windows-msvc"),
            Some("windows-arm64")
        );
        assert_eq!(
            catalogue_slug_for("x86_64-apple-darwin"),
            Some("darwin-x64")
        );
        assert_eq!(
            catalogue_slug_for("aarch64-apple-darwin"),
            Some("darwin-arm64")
        );
        assert_eq!(
            catalogue_slug_for("x86_64-unknown-linux-musl"),
            Some("linux-x64-musl")
        );
        assert_eq!(catalogue_slug_for("wasm32-unknown-unknown"), None);
    });

    crate::timed_test!(resolve_python_version_env_var_serial, {
        let prev = std::env::var_os(PYTHON_VERSION_ENV_VAR);

        std::env::remove_var(PYTHON_VERSION_ENV_VAR);
        assert_eq!(resolve_python_version(), DEFAULT_PYTHON_VERSION);

        std::env::set_var(PYTHON_VERSION_ENV_VAR, "3.12.7");
        assert_eq!(resolve_python_version(), "3.12.7");

        std::env::set_var(PYTHON_VERSION_ENV_VAR, "99.99");
        assert_eq!(
            resolve_python_version(),
            DEFAULT_PYTHON_VERSION,
            "unsupported version must fall back to default"
        );

        match prev {
            Some(v) => std::env::set_var(PYTHON_VERSION_ENV_VAR, v),
            None => std::env::remove_var(PYTHON_VERSION_ENV_VAR),
        }
    });

    crate::timed_test!(supported_versions_include_default, {
        assert!(SUPPORTED_PYTHON_VERSIONS.contains(&DEFAULT_PYTHON_VERSION));
    });
}
