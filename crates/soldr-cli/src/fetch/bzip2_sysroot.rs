//! bzip2 (libbz2) sysroot fetcher — soldr#1064 Phase B.
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
//! When `bzip2-sys` is in the transitive deps and the catalogue row is
//! ingested, the blessed path exports `PKG_CONFIG_PATH` pointing at the
//! materialized `lib/pkgconfig/` directory so `bzip2-sys`' build.rs
//! uses pkg-config to locate the precompiled libbz2 instead of
//! recompiling the in-tree bzip2 sources (1-3s).
//!
//! Lowest-cost crate in the in-scope list — included for completeness
//! since the recipe pattern is identical.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

pub const MANAGED_BZIP2_VERSION: &str = "1.0.8";

pub const BZIP2_TARGETS: &[(&str, &str)] = &[
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
    BZIP2_TARGETS
        .iter()
        .find(|(rust, _)| *rust == triple)
        .map(|(_, slug)| *slug)
}

pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("bzip2", version, slug)
}

pub async fn ensure_bzip2_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no bzip2 sysroot recipe for target {target_triple}; \
             supported: {:?}",
            BZIP2_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(paths, "bzip2", MANAGED_BZIP2_VERSION, slug).await
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
            catalogue_slug_for("x86_64-pc-windows-gnu"),
            Some("windows-x64-gnu")
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
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(ensure_bzip2_sysroot_rejects_unknown_target, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_bzip2_sysroot(&paths, "wasm32-unknown-unknown"));
        let err = result.expect_err("unsupported target must error");
        assert!(matches!(err, SoldrError::UnsupportedPlatform(_)));
    });
}
