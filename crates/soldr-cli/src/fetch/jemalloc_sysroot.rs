//! jemalloc sysroot fetcher — soldr#1064 Phase B.
//!
//! Consumes the soldr-toolchain `recipes/jemalloc-<platform>/`
//! catalogue rows. Each row ships:
//!
//! ```
//! lib/libjemalloc.a    (the load-bearing file)
//! lib/libjemalloc_pic.a
//! include/jemalloc/*.h
//! ```
//!
//! When `tikv-jemalloc-sys` is in the transitive deps and the
//! catalogue row is ingested, the blessed path exports:
//!
//!   * `JEMALLOC_OVERRIDE=<sysroot>/lib/libjemalloc.a`
//!
//! tikv-jemalloc-sys' build.rs short-circuits the 20-40s autotools
//! build entirely when this env var points at a static archive.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

/// Pinned jemalloc version the soldr-toolchain `jemalloc-*` recipes
/// ship.
pub const MANAGED_JEMALLOC_VERSION: &str = "5.3.0";

/// Catalogue layout: Rust target triple → recipe slug.
/// Note: jemalloc does NOT support Windows MSVC — upstream
/// build system is autotools-only. Windows targets are
/// intentionally absent from this table.
pub const JEMALLOC_TARGETS: &[(&str, &str)] = &[
    ("x86_64-apple-darwin", "darwin-x64"),
    ("aarch64-apple-darwin", "darwin-arm64"),
    ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
    ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
    ("x86_64-unknown-linux-musl", "linux-x64-musl"),
    ("aarch64-unknown-linux-musl", "linux-arm64-musl"),
];

pub fn catalogue_slug_for(triple: &str) -> Option<&'static str> {
    JEMALLOC_TARGETS
        .iter()
        .find(|(rust, _)| *rust == triple)
        .map(|(_, slug)| *slug)
}

pub fn asset_url_for(version: &str, slug: &str) -> String {
    format!(
        "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/\
         deps/jemalloc/{version}/{slug}/bundle.tar.zst"
    )
}

pub async fn ensure_jemalloc_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let _ = paths;
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no jemalloc sysroot recipe for target {target_triple} \
             (note: jemalloc has no Windows support); supported: {:?}",
            JEMALLOC_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    let url = asset_url_for(MANAGED_JEMALLOC_VERSION, slug);
    Err(SoldrError::Other(format!(
        "jemalloc sysroot for {target_triple} ({slug}) not yet ingested into the \
         soldr-toolchain catalogue. Expected URL: {url}\n\
         Tracking: https://github.com/zackees/soldr/issues/1064"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(slug_for_supported_triples, {
        assert_eq!(
            catalogue_slug_for("x86_64-unknown-linux-musl"),
            Some("linux-x64-musl")
        );
        assert_eq!(
            catalogue_slug_for("aarch64-apple-darwin"),
            Some("darwin-arm64")
        );
        // Windows is intentionally unsupported for jemalloc.
        assert_eq!(catalogue_slug_for("x86_64-pc-windows-msvc"), None);
    });

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let u = asset_url_for(MANAGED_JEMALLOC_VERSION, "linux-x64-musl");
        assert!(u.contains("/deps/jemalloc/5.3.0/linux-x64-musl/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(ensure_jemalloc_sysroot_returns_not_yet_ingested, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_jemalloc_sysroot(&paths, "x86_64-unknown-linux-musl"));
        let err = result.expect_err("must error until catalogue row lands");
        assert!(err.to_string().contains("not yet ingested"));
    });

    crate::timed_test!(ensure_jemalloc_rejects_windows, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_jemalloc_sysroot(&paths, "x86_64-pc-windows-msvc"));
        let err = result.expect_err("jemalloc must reject windows");
        assert!(matches!(err, SoldrError::UnsupportedPlatform(_)));
    });
}
