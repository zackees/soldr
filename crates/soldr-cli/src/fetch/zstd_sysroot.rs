//! zstd (libzstd) sysroot fetcher — soldr#1064 Phase B.
//!
//! Consumes the soldr-toolchain `recipes/zstd-<platform>/` catalogue
//! rows. Each row ships:
//!
//! ```
//! lib/libzstd.{a,lib}
//! lib/pkgconfig/libzstd.pc
//! include/{zstd.h, zdict.h, zstd_errors.h}
//! ```
//!
//! When the workspace's transitive deps include `zstd-sys` and the
//! catalogue row for the active target is ingested, the blessed build
//! path exports:
//!
//!   * `ZSTD_SYS_USE_PKG_CONFIG=1`
//!   * `PKG_CONFIG_PATH=<sysroot>/lib/pkgconfig:$PKG_CONFIG_PATH`
//!
//! so `zstd-sys`' build script links against the precompiled libzstd
//! instead of recompiling 11-15s of C source on every fresh checkout.
//!
//! Mirrors [`super::openssl_sysroot`]'s stub-until-ingested shape.
//! Asset population happens via the soldr-toolchain forge pipeline;
//! until those rows land, `ensure_zstd_sysroot` returns the
//! "not yet ingested" error and the caller falls through to the
//! crate's vendored compile.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

/// Pinned libzstd version the soldr-toolchain `zstd-*` recipes ship.
/// Bump alongside the recipe dispatch + the Cargo.lock entry for
/// `zstd-sys`.
pub const MANAGED_ZSTD_VERSION: &str = "1.5.7";

/// Catalogue layout: Rust target triple → recipe slug.
pub const ZSTD_TARGETS: &[(&str, &str)] = &[
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
    ZSTD_TARGETS
        .iter()
        .find(|(rust, _)| *rust == triple)
        .map(|(_, slug)| *slug)
}

/// Construct the expected `assets`-branch URL. Thin wrapper over
/// [`super::syslib_common::asset_url_for`] kept for the existing
/// per-module test contract.
pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("zstd", version, slug)
}

pub async fn ensure_zstd_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no zstd sysroot recipe for target {target_triple}; \
             supported: {:?}",
            ZSTD_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(paths, "zstd", MANAGED_ZSTD_VERSION, slug).await
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
            catalogue_slug_for("x86_64-unknown-linux-gnu"),
            Some("linux-x64-gnu")
        );
        assert_eq!(
            catalogue_slug_for("aarch64-unknown-linux-gnu"),
            Some("linux-arm64-gnu")
        );
        assert_eq!(
            catalogue_slug_for("x86_64-unknown-linux-musl"),
            Some("linux-x64-musl")
        );
        assert_eq!(
            catalogue_slug_for("aarch64-unknown-linux-musl"),
            Some("linux-arm64-musl")
        );
        assert_eq!(catalogue_slug_for("wasm32-unknown-unknown"), None);
    });

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let u = asset_url_for(MANAGED_ZSTD_VERSION, "linux-x64-musl");
        assert!(u.contains("/zstd/1.5.7/linux-x64-musl/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    // The original `ensure_zstd_sysroot_returns_not_yet_ingested` unit
    // test was removed in the soldr#1064 phase B follow-up: now that the
    // zstd 1.5.7 bundle IS ingested into the live catalogue, this
    // function returns either a real sysroot path (catalogue reachable)
    // or a network error (catalogue unreachable). Neither is a stable
    // unit-test signal. The end-to-end smoke is exercised in the cross-
    // compile lanes that run `cargo build` against the *-sys crate.
}
