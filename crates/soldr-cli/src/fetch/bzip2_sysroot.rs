//! bzip2 (libbz2) sysroot fetcher placeholder for soldr#1064 Phase B.
//!
//! Consumes the soldr-toolchain `recipes/bzip2-<platform>/` catalogue
//! rows. Each row ships:
//!
//! ```
//! lib/libbz2.{a,lib}
//! lib/pkgconfig/bzip2.pc
//! include/bzlib.h
//! ```
//!
//! This module is intentionally disabled for now. `bzip2-sys` only has
//! partial pkg-config behavior across the target set, and
//! `BZIP2_NO_PKG_CONFIG=0` is not a valid enable contract for the
//! all-target blessed-build path tracked by soldr#1064.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

pub const MANAGED_BZIP2_VERSION: &str = "1.0.8";

pub const BZIP2_TARGETS: &[(&str, &str)] = &[
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
    BZIP2_TARGETS
        .iter()
        .find(|(rust, _)| *rust == triple)
        .map(|(_, slug)| *slug)
}

pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_bundle::asset_url_for("bzip2", version, slug)
}

pub async fn ensure_bzip2_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let _ = paths;
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no bzip2 sysroot recipe for target {target_triple}; supported: {:?}",
            BZIP2_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    let url = asset_url_for(MANAGED_BZIP2_VERSION, slug);
    Err(SoldrError::Other(format!(
        "bzip2 sysroot for {target_triple} ({slug}) is disabled: current bzip2-sys \
         does not expose a valid all-target catalogue override. Current expected \
         URL: {url}\n\
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
            catalogue_slug_for("aarch64-apple-darwin"),
            Some("darwin-arm64")
        );
        assert_eq!(catalogue_slug_for("wasm32-unknown-unknown"), None);
    });

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let u = asset_url_for(MANAGED_BZIP2_VERSION, "linux-x64-gnu");
        assert!(u.contains("/bzip2/1.0.8/linux-x64-gnu/"));
        assert!(!u.contains("/deps/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(ensure_bzip2_sysroot_returns_disabled_error, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_bzip2_sysroot(&paths, "x86_64-unknown-linux-gnu"));
        let err = result.expect_err("must error while disabled");
        assert!(err.to_string().contains("disabled"));
    });
}
