//! lzma (xz / liblzma) sysroot fetcher — soldr#1064 Phase B.
//!
//! Consumes the soldr-toolchain `recipes/lzma-<platform>/` catalogue
//! rows. Each row ships:
//!
//! ```
//! lib/liblzma.{a,lib}
//! lib/pkgconfig/liblzma.pc
//! include/lzma.h
//! include/lzma/*.h
//! ```
//!
//! When `lzma-sys` is in the transitive deps and the catalogue row is
//! ingested, the blessed path exports:
//!
//!   * `LZMA_API_STATIC=1`
//!   * `PKG_CONFIG_PATH=<sysroot>/lib/pkgconfig:$PKG_CONFIG_PATH`
//!
//! so `lzma-sys`' build.rs uses pkg-config to locate the precompiled
//! liblzma static archive instead of recompiling its in-tree xz copy
//! (5-10s).

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

pub const MANAGED_LZMA_VERSION: &str = "5.6.3";

pub const LZMA_TARGETS: &[(&str, &str)] = &[
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
    LZMA_TARGETS
        .iter()
        .find(|(rust, _)| *rust == triple)
        .map(|(_, slug)| *slug)
}

pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("lzma", version, slug)
}

pub async fn ensure_lzma_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no lzma sysroot recipe for target {target_triple}; \
             supported: {:?}",
            LZMA_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(paths, "lzma", MANAGED_LZMA_VERSION, slug).await
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
            catalogue_slug_for("aarch64-apple-darwin"),
            Some("darwin-arm64")
        );
        assert_eq!(catalogue_slug_for("wasm32-unknown-unknown"), None);
    });

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let u = asset_url_for(MANAGED_LZMA_VERSION, "darwin-arm64");
        assert!(u.contains("/lzma/5.6.3/darwin-arm64/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(ensure_lzma_sysroot_rejects_unknown_target, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_lzma_sysroot(&paths, "wasm32-unknown-unknown"));
        let err = result.expect_err("unsupported target must error");
        assert!(matches!(err, SoldrError::UnsupportedPlatform(_)));
    });
}
