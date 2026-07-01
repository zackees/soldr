//! mimalloc sysroot fetcher — soldr#1064 Phase B.
//!
//! Consumes the soldr-toolchain `recipes/mimalloc-<platform>/`
//! catalogue rows. Each row ships:
//!
//! ```
//! lib/libmimalloc.{a,lib}
//! include/mimalloc.h
//! ```
//!
//! When `libmimalloc-sys` is in the transitive deps and the catalogue
//! row is ingested, the blessed path injects Cargo build-script
//! override config for `links = "mimalloc"`:
//!
//!   * `target.<triple>.mimalloc.rustc-link-lib=["static=mimalloc"]`
//!   * `target.<triple>.mimalloc.rustc-link-search=["<sysroot>/lib"]`
//!   * `target.<triple>.mimalloc.metadata_include_dir="<sysroot>/include"`
//!
//! That target-scoped Cargo config skips `libmimalloc-sys`' build.rs
//! entirely. This is deliberately not an environment variable:
//! `libmimalloc-sys` v0.1.49 has no `MIMALLOC_OVERRIDE` hook.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

/// Pinned mimalloc version vendored by `libmimalloc-sys` 0.1.49.
/// `c_src/mimalloc/v3/include/mimalloc.h` reports MI_MALLOC_VERSION
/// 30302, i.e. upstream mimalloc 3.3.2.
pub const MANAGED_MIMALLOC_VERSION: &str = "3.3.2";

pub const MIMALLOC_TARGETS: &[(&str, &str)] = &[
    ("x86_64-pc-windows-msvc", "windows-x64"),
    ("aarch64-pc-windows-msvc", "windows-arm64"),
    ("x86_64-apple-darwin", "darwin-x64"),
    ("aarch64-apple-darwin", "darwin-arm64"),
    ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
    ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
    ("x86_64-unknown-linux-musl", "linux-x64-musl"),
    ("aarch64-unknown-linux-musl", "linux-arm64-musl"),
];

pub fn catalogue_slug_for(triple: &str) -> Option<&'static str> {
    MIMALLOC_TARGETS
        .iter()
        .find(|(rust, _)| *rust == triple)
        .map(|(_, slug)| *slug)
}

pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("mimalloc", version, slug)
}

pub async fn ensure_mimalloc_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no mimalloc sysroot recipe for target {target_triple}; \
             supported: {:?}",
            MIMALLOC_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(paths, "mimalloc", MANAGED_MIMALLOC_VERSION, slug)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(slug_for_supported_triples, {
        assert_eq!(
            catalogue_slug_for("x86_64-pc-windows-msvc"),
            Some("windows-x64")
        );
        assert_eq!(
            catalogue_slug_for("aarch64-unknown-linux-gnu"),
            Some("linux-arm64-gnu")
        );
        assert_eq!(catalogue_slug_for("wasm32-unknown-unknown"), None);
    });

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let u = asset_url_for(MANAGED_MIMALLOC_VERSION, "windows-x64");
        assert!(u.contains("/mimalloc/3.3.2/windows-x64/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    // No live "not yet ingested" assertion here: mimalloc rows are
    // expected to appear in the catalogue as part of soldr#1064.
    // Once they do, ensure_mimalloc_sysroot returns a real sysroot or a
    // network error, neither of which is a stable unit-test signal.
}
