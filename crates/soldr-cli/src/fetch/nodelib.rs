//! Node.js node.lib fetcher — Phase A of soldr#997 (closes #944).
//!
//! Native Node addons (napi-rs, neon, raw N-API in Rust) need
//! `node.lib` (the import library for `node.exe`) at link time on
//! Windows MSVC cross-compile. The soldr-toolchain
//! `recipes/nodelib-windows-{x64,arm64}/` recipes pull the official
//! Node.js dist `headers.tar.gz` + the matching per-arch `node.lib`,
//! and the soldr-toolchain ingest pipeline republishes them under
//!
//!     nodelib/<node-version>/<slug>/bundle.tar.zst
//!
//! This module's `ensure_nodelib_sysroot` materializes the bundle and
//! returns the directory containing `include/` + `lib/node.lib`. The
//! cargo front door uses it (#939 stage 2 follow-up) to set:
//!
//!   * `npm_config_target=<node-version>`
//!   * `npm_config_arch=<x64|arm64>`
//!   * `npm_config_runtime=node`
//!   * `npm_config_target_libdir=<bundle>/lib`
//!
//! so the `*-sys` crates that look for `node.lib` find it.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

/// Pinned Node.js version the soldr-toolchain `nodelib-windows-*`
/// recipes ship. Node 22 is the active LTS at the time of authoring.
pub const MANAGED_NODE_VERSION: &str = "22.10.0";

/// Env-var override — pin Node's version. Useful for downstream
/// projects targeting an LTS line different from the bundled default.
pub const NODE_VERSION_ENV_VAR: &str = "SOLDR_NODE_VERSION";

pub const NODELIB_TARGETS: &[(&str, &str)] = &[
    ("x86_64-pc-windows-msvc", "windows-x64"),
    ("aarch64-pc-windows-msvc", "windows-arm64"),
];

pub fn catalogue_slug_for(triple: &str) -> Option<&'static str> {
    NODELIB_TARGETS
        .iter()
        .find(|(rust, _)| *rust == triple)
        .map(|(_, slug)| *slug)
}

pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("nodelib", version, slug)
}

pub fn resolve_node_version() -> String {
    if let Ok(value) = std::env::var(NODE_VERSION_ENV_VAR) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    MANAGED_NODE_VERSION.to_string()
}

pub async fn ensure_nodelib_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no nodelib recipe for target {target_triple}; \
             supported: {:?}",
            NODELIB_TARGETS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    let version = resolve_node_version();
    super::syslib_common::ensure_syslib_bundle(paths, "nodelib", &version, slug).await
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
        assert_eq!(catalogue_slug_for("x86_64-apple-darwin"), None);
    });

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let u = asset_url_for(MANAGED_NODE_VERSION, "windows-x64");
        assert!(u.contains("/nodelib/22.10.0/windows-x64/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(resolve_node_version_env_var_serial, {
        let prev = std::env::var_os(NODE_VERSION_ENV_VAR);

        std::env::remove_var(NODE_VERSION_ENV_VAR);
        assert_eq!(resolve_node_version(), MANAGED_NODE_VERSION);

        std::env::set_var(NODE_VERSION_ENV_VAR, "20.18.0");
        assert_eq!(resolve_node_version(), "20.18.0");

        match prev {
            Some(v) => std::env::set_var(NODE_VERSION_ENV_VAR, v),
            None => std::env::remove_var(NODE_VERSION_ENV_VAR),
        }
    });
}
