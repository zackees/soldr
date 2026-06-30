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
//! This module is intentionally disabled for now. The current
//! `libmimalloc-sys` build script does not expose a
//! `MIMALLOC_OVERRIDE` contract, and its bundled mimalloc version does
//! not match the merged catalogue recipe. Do not wire this into
//! blessed builds until the upstream hook and version are aligned.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

pub const MANAGED_MIMALLOC_VERSION: &str = "3.0.4";

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
    super::syslib_bundle::asset_url_for("mimalloc", version, slug)
}

pub async fn ensure_mimalloc_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let _ = paths;
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no mimalloc sysroot recipe for target {target_triple}; \
             supported: {:?}",
            MIMALLOC_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    let url = asset_url_for(MANAGED_MIMALLOC_VERSION, slug);
    Err(SoldrError::Other(format!(
        "mimalloc sysroot for {target_triple} ({slug}) is disabled: current \
         libmimalloc-sys does not expose a MIMALLOC_OVERRIDE hook and the \
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
            catalogue_slug_for("aarch64-unknown-linux-gnu"),
            Some("linux-arm64-gnu")
        );
        assert_eq!(catalogue_slug_for("wasm32-unknown-unknown"), None);
    });

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let u = asset_url_for(MANAGED_MIMALLOC_VERSION, "windows-x64");
        assert!(u.contains("/mimalloc/3.0.4/windows-x64/"));
        assert!(!u.contains("/deps/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(ensure_mimalloc_sysroot_returns_disabled_error, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_mimalloc_sysroot(&paths, "aarch64-apple-darwin"));
        let err = result.expect_err("must error while disabled");
        assert!(err.to_string().contains("disabled"));
    });
}
