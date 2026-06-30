//! zlib-ng sysroot fetcher — soldr#1064 Phase B.
//!
//! Consumes the soldr-toolchain `recipes/zlib-ng-<platform>/`
//! catalogue rows. Each row ships:
//!
//! ```
//! lib/libz-ng.{a,lib}
//! lib/pkgconfig/zlib-ng.pc
//! include/zlib-ng.h
//! ```
//!
//! This module is intentionally disabled for now. The current
//! `libz-ng-sys` version does not expose a reliable system-library
//! override for these catalogue bundles, and its vendored zlib-ng
//! version does not match the merged recipe. Blessed builds must not
//! inject this sysroot until that contract is real.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

pub const MANAGED_ZLIB_NG_VERSION: &str = "2.2.5";

pub const ZLIB_NG_TARGETS: &[(&str, &str)] = &[
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
    ZLIB_NG_TARGETS
        .iter()
        .find(|(rust, _)| *rust == triple)
        .map(|(_, slug)| *slug)
}

pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_bundle::asset_url_for("zlib-ng", version, slug)
}

pub async fn ensure_zlib_ng_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let _ = paths;
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no zlib-ng sysroot recipe for target {target_triple}; \
             supported: {:?}",
            ZLIB_NG_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    let url = asset_url_for(MANAGED_ZLIB_NG_VERSION, slug);
    Err(SoldrError::Other(format!(
        "zlib-ng sysroot for {target_triple} ({slug}) is disabled: current \
         libz-ng-sys does not expose a reliable catalogue override and the \
         catalogue recipe version is not aligned. Current expected URL: {url}\n\
         Tracking: https://github.com/zackees/soldr/issues/1064"
    )))
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
            catalogue_slug_for("aarch64-unknown-linux-musl"),
            Some("linux-arm64-musl")
        );
        assert_eq!(catalogue_slug_for("wasm32-unknown-unknown"), None);
    });

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let u = asset_url_for(MANAGED_ZLIB_NG_VERSION, "linux-x64-gnu");
        assert!(u.contains("/zlib-ng/2.2.5/linux-x64-gnu/"));
        assert!(!u.contains("/deps/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(ensure_zlib_ng_sysroot_returns_disabled_error, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_zlib_ng_sysroot(&paths, "x86_64-unknown-linux-gnu"));
        let err = result.expect_err("must error while disabled");
        assert!(err.to_string().contains("disabled"));
    });
}
