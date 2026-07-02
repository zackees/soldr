//! Managed `uv` bundle fetcher — soldr#1264 follow-on.
//!
//! `uv` (astral-sh/uv) joins the soldr-toolchain archive as a managed
//! host tool so soldr can provision Python tooling in manually-managed
//! isolated environments — first consumer: the maturin fallback in
//! [`super::maturin_env`], which does `uv venv` + `uv pip install
//! maturin==<pin>` when the prebuilt maturin binary fetch misses.
//! Deliberately NOT the `uv-iso-env` PyPI package: the whole point is
//! zero Python-level dependencies in soldr's build backend, so the
//! isolation dance is done by hand against a pinned managed binary.
//!
//! Bundle layout (forge recipes `uv-<shape>` on zackees/soldr-toolchain):
//!
//! ```text
//! bin/uv(.exe)
//! bin/uvx(.exe)   ← present in upstream archives; shipped along
//! ```
//!
//! Unlike cmake/ninja, upstream ships musl builds, so all 8 standard
//! shapes are supported. Stub-until-ingested like every catalogue
//! consumer: a catalogue miss errors and callers fall through.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

/// uv version pinned by the soldr-toolchain `uv-<shape>` recipes.
pub const MANAGED_UV_VERSION: &str = "0.11.26";

/// Host triple → catalogue shape slug. uv is a HOST tool.
pub const UV_TOOL_HOSTS: &[(&str, &str)] = &[
    ("x86_64-pc-windows-msvc", "windows-x64"),
    ("aarch64-pc-windows-msvc", "windows-arm64"),
    ("x86_64-apple-darwin", "darwin-x64"),
    ("aarch64-apple-darwin", "darwin-arm64"),
    ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
    ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
    ("x86_64-unknown-linux-musl", "linux-x64-musl"),
    ("aarch64-unknown-linux-musl", "linux-arm64-musl"),
];

/// Catalogue slug for a host triple, if supported.
pub fn host_slug_for(host_triple: &str) -> Option<&'static str> {
    UV_TOOL_HOSTS
        .iter()
        .find(|(rust, _)| *rust == host_triple)
        .map(|(_, slug)| *slug)
}

/// Assets-branch URL for the uv bundle. Catalogue layout:
/// `uv/<version>/<slug>/bundle.tar.zst`.
pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("uv", version, slug)
}

/// Materialize the managed uv bundle for `host_triple`. Returns the
/// bundle root (contains `bin/`).
pub async fn ensure_uv_bundle(
    paths: &SoldrPaths,
    host_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = host_slug_for(host_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no managed uv bundle for host {host_triple}; supported: {:?}",
            UV_TOOL_HOSTS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(paths, "uv", MANAGED_UV_VERSION, slug).await
}

/// Path to the uv executable inside a materialized bundle root.
pub fn uv_exe(bundle_root: &Path) -> PathBuf {
    let name = if cfg!(windows) { "uv.exe" } else { "uv" };
    bundle_root.join("bin").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(uv_host_slug_covers_all_eight_shapes, {
        assert_eq!(host_slug_for("x86_64-pc-windows-msvc"), Some("windows-x64"));
        assert_eq!(
            host_slug_for("x86_64-unknown-linux-musl"),
            Some("linux-x64-musl"),
            "uv ships musl builds — musl hosts are supported (unlike cmake/ninja)"
        );
        assert_eq!(
            host_slug_for("aarch64-unknown-linux-musl"),
            Some("linux-arm64-musl")
        );
        assert_eq!(host_slug_for("wasm32-unknown-unknown"), None);
        assert_eq!(UV_TOOL_HOSTS.len(), 8);
    });

    crate::timed_test!(uv_asset_url_layout_matches_catalogue, {
        let u = asset_url_for(MANAGED_UV_VERSION, "windows-x64");
        assert!(u.starts_with("https://media.githubusercontent.com/media/"));
        assert!(u.contains("/uv/0.11.26/windows-x64/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(uv_version_constant_well_formed, {
        let parts: Vec<&str> = MANAGED_UV_VERSION.split('.').collect();
        assert_eq!(parts.len(), 3);
        for p in parts {
            assert!(p.chars().all(|c| c.is_ascii_digit()));
        }
    });

    crate::timed_test!(uv_exe_path_is_platform_correct, {
        let exe = uv_exe(Path::new("root"));
        if cfg!(windows) {
            assert!(exe.ends_with("bin/uv.exe") || exe.ends_with("bin\\uv.exe"));
        } else {
            assert!(exe.ends_with("bin/uv"));
        }
    });

    crate::timed_test!(ensure_uv_bundle_rejects_unsupported_host, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_uv_bundle(&paths, "wasm32-unknown-unknown"))
            .expect_err("unsupported host must error");
        assert!(matches!(err, SoldrError::UnsupportedPlatform(_)));
    });
}
