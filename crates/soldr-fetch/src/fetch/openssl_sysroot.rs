//! Managed static OpenSSL sysroot fetcher (soldr#3246).
//!
//! Consumes the soldr-toolchain `recipes/openssl-<shape>/` catalogue rows.
//! The bundles are **static and source-built**: forge compiles upstream
//! OpenSSL with `no-shared` for every soldr shape, so each row ships
//!
//! ```text
//! lib/{libssl,libcrypto}.lib              (*-pc-windows-msvc)
//! lib/{libssl,libcrypto}.a                (every other shape)
//! lib/pkgconfig/{libssl,libcrypto,openssl}.pc   relocatable prefix
//! include/openssl/*.h
//! ```
//!
//! and no DLL or dylib. The earlier 3.5.0 rows repackaged FireDaemon's DLL
//! build for Windows only (soldr#943); this module no longer points at them.
//!
//! When `links = "openssl"` resolves to `openssl-sys` for the target,
//! `blessed_build`'s OpenSSL override exports the target-scoped
//! `<T>_OPENSSL_DIR`, `<T>_OPENSSL_NO_VENDOR=1` and `<T>_OPENSSL_STATIC=1`,
//! so `openssl-sys` links this sysroot instead of running `openssl-src`
//! (which cannot build `*-pc-windows-msvc` from a non-Windows host).
//!
//! Every download is sha256-verified against the toolchain catalogue by
//! [`super::syslib_common::ensure_syslib_bundle`].

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

/// Pinned OpenSSL version the soldr-toolchain `openssl-*` recipes build
/// (3.5 LTS). Bump alongside the recipe dispatch.
pub const MANAGED_OPENSSL_VERSION: &str = "3.5.8";

/// Catalogue layout: Rust target triple → recipe slug. Same nine shapes as
/// every other `*-sys` syslib.
pub const OPENSSL_TARGETS: &[(&str, &str)] = &[
    ("x86_64-pc-windows-msvc", "windows-x64"),
    ("x86_64-pc-windows-gnu", "windows-x64-gnu"),
    ("aarch64-pc-windows-msvc", "windows-arm64"),
    ("x86_64-apple-darwin", "darwin-x64"),
    ("aarch64-apple-darwin", "darwin-arm64"),
    ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
    ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
    ("x86_64-unknown-linux-musl", "linux-x64-musl"),
    ("aarch64-unknown-linux-musl", "linux-arm64-musl"),
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

    #[test]
    fn slug_for_supported_triples() {
        let expected = [
            ("x86_64-pc-windows-msvc", "windows-x64"),
            ("aarch64-pc-windows-msvc", "windows-arm64"),
            ("x86_64-pc-windows-gnu", "windows-x64-gnu"),
            ("x86_64-apple-darwin", "darwin-x64"),
            ("aarch64-apple-darwin", "darwin-arm64"),
            ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
            ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
            ("x86_64-unknown-linux-musl", "linux-x64-musl"),
            ("aarch64-unknown-linux-musl", "linux-arm64-musl"),
        ];
        for (triple, slug) in expected {
            assert_eq!(catalogue_slug_for(triple), Some(slug), "{triple}");
        }
        assert_eq!(OPENSSL_TARGETS.len(), expected.len());
        assert_eq!(catalogue_slug_for("wasm32-unknown-unknown"), None);
    }

    #[test]
    fn targets_match_the_shared_syslib_shapes() {
        // The OpenSSL recipes are generated for the same nine shapes as
        // the other syslibs; a drift here would silently skip a target.
        assert_eq!(OPENSSL_TARGETS, super::super::zstd_sysroot::ZSTD_TARGETS);
    }

    #[test]
    fn asset_url_layout_matches_catalogue() {
        let u = asset_url_for(MANAGED_OPENSSL_VERSION, "windows-arm64");
        assert!(u.contains("/openssl/3.5.8/windows-arm64/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    }

    #[test]
    fn ensure_openssl_sysroot_rejects_unknown_target() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_openssl_sysroot(&paths, "wasm32-unknown-unknown"));
        let err = result.expect_err("unsupported target must error");
        assert!(matches!(err, SoldrError::UnsupportedPlatform(_)));
    }

    #[test]
    fn a_completed_bundle_is_reused_without_a_fetch() {
        // The stamp short-circuit is what lets callers seed a fake bundle
        // in no-network tests; pin it for the OpenSSL layout.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let install_root = paths
            .bin
            .join("syslib")
            .join("openssl")
            .join(MANAGED_OPENSSL_VERSION)
            .join("windows-x64");
        std::fs::create_dir_all(install_root.join("package")).expect("package dir");
        std::fs::write(install_root.join(".complete"), "test").expect("stamp");
        let sysroot = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_openssl_sysroot(&paths, "x86_64-pc-windows-msvc"))
            .expect("seeded bundle");
        assert_eq!(sysroot, install_root.join("package"));
    }
}
