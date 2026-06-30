//! OpenSSL Windows MSVC sysroot fetcher — Phase A of soldr#997
//! (closes #943).
//!
//! Consumes the `recipes/openssl-windows-{x64,arm64}/` catalogue rows.
//! Each row ships:
//!
//! ```
//! bin/{libssl-3-x64.dll, libcrypto-3-x64.dll, openssl.exe}
//! lib/{libssl.lib, libcrypto.lib}      ← MSVC import libs
//! include/openssl/*.h
//! ```
//!
//! Source: upstream OpenSSL 3.x compiled by FireDaemon, repackaged by
//! the soldr-toolchain forge pipeline. See
//! `soldr-toolchain/recipes/_openssl_firedaemon.py` for the producer
//! side; this module is the soldr-side consumer.
//!
//! Cargo front door (#939 stage 2 follow-up) reads this sysroot when
//! the workspace's transitive deps include `openssl-sys` and the target
//! is `*-pc-windows-msvc`, then exports:
//!   * `OPENSSL_DIR=<sysroot>`
//!   * `OPENSSL_STATIC=1`
//! so `openssl-sys`' build script resolves the bundled libs instead of
//! triggering the slow `vendored` build-from-source.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

/// Pinned OpenSSL version the soldr-toolchain `openssl-windows-*`
/// recipes ship. Bump alongside the recipe dispatch.
pub const MANAGED_OPENSSL_VERSION: &str = "3.5.0";

/// Catalogue layout: Rust target triple → recipe slug.
pub const OPENSSL_TARGETS: &[(&str, &str)] = &[
    ("x86_64-pc-windows-msvc", "windows-x64"),
    ("aarch64-pc-windows-msvc", "windows-arm64"),
];

pub fn catalogue_slug_for(triple: &str) -> Option<&'static str> {
    OPENSSL_TARGETS
        .iter()
        .find(|(rust, _)| *rust == triple)
        .map(|(_, slug)| *slug)
}

/// Construct the expected `assets`-branch URL via the shared
/// `syslib_common` helper. Layout:
/// `https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/openssl/<version>/<slug>/bundle.tar.zst`.
pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("openssl", version, slug)
}

pub async fn ensure_openssl_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no openssl sysroot recipe for target {target_triple}; \
             supported: {:?}",
            OPENSSL_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(paths, "openssl", MANAGED_OPENSSL_VERSION, slug)
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
            catalogue_slug_for("aarch64-pc-windows-msvc"),
            Some("windows-arm64")
        );
        assert_eq!(catalogue_slug_for("x86_64-unknown-linux-gnu"), None);
    });

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let u = asset_url_for(MANAGED_OPENSSL_VERSION, "windows-arm64");
        assert!(u.contains("/openssl/3.5.0/windows-arm64/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });
}
