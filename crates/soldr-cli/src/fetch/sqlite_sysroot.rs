//! SQLite (libsqlite3) sysroot fetcher — soldr#1064 Phase B.
//!
//! Consumes the soldr-toolchain `recipes/sqlite-<platform>/` catalogue
//! rows. Each row ships:
//!
//! ```
//! lib/libsqlite3.{a,lib}
//! lib/pkgconfig/sqlite3.pc
//! include/{sqlite3.h, sqlite3ext.h}
//! ```
//!
//! When `libsqlite3-sys` is in the transitive deps and the catalogue
//! row is ingested, the blessed path exports:
//!
//!   * `LIBSQLITE3_SYS_USE_PKG_CONFIG=1`
//!   * `PKG_CONFIG_PATH=<sysroot>/lib/pkgconfig:$PKG_CONFIG_PATH`
//!
//! This is the highest-cost crate in the in-scope list (35-47s per
//! lane for the SQLite amalgamation compile). Mirrors
//! [`super::openssl_sysroot`]'s stub-until-ingested shape.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

/// Pinned SQLite version the soldr-toolchain `sqlite-*` recipes ship.
/// Bump alongside the recipe dispatch + the Cargo.lock entry for
/// `libsqlite3-sys`.
pub const MANAGED_SQLITE_VERSION: &str = "3.46.0";

/// Catalogue layout: Rust target triple → recipe slug.
pub const SQLITE_TARGETS: &[(&str, &str)] = &[
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
    SQLITE_TARGETS
        .iter()
        .find(|(rust, _)| *rust == triple)
        .map(|(_, slug)| *slug)
}

pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("sqlite", version, slug)
}

pub async fn ensure_sqlite_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no sqlite sysroot recipe for target {target_triple}; \
             supported: {:?}",
            SQLITE_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(paths, "sqlite", MANAGED_SQLITE_VERSION, slug).await
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
        let u = asset_url_for(MANAGED_SQLITE_VERSION, "linux-arm64-musl");
        assert!(u.contains("/sqlite/3.46.0/linux-arm64-musl/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    // The original `ensure_sqlite_sysroot_returns_not_yet_ingested` test
    // was removed in the soldr#1064 phase B follow-up: sqlite 3.46.0 IS
    // ingested into the live catalogue, so this function now returns a
    // real sysroot path or a network error. End-to-end coverage runs in
    // the cross-compile lanes that build against `libsqlite3-sys`.
}
