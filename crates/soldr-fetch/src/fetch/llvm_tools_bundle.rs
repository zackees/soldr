//! LLVM toolchain bundle fetcher — Phase A of soldr#997 (closes #934 +
//! #942).
//!
//! Distinct from [`crate::fetch::llvm`]: that module pulls the existing
//! `zackees/clang-tool-chain-bins` LLVM build (the MSVC cross-compile
//! bootstrap for blessed `soldr build` and explicit cargo-xwin
//! fallback). This module
//! consumes the **new** `recipes/llvm-tools-linux-x64/` catalogue row
//! that bundles the same upstream LLVM 20.1.7 release archive
//! whitelist-extracted to just the binutils we actually need:
//!
//! ```
//! bin/{clang, clang++, clang-cl, lld, lld-link, llvm-lib, llvm-rc,
//!      llvm-dlltool, llvm-strip, llvm-objcopy, llvm-ar, llvm-readobj}
//! lib/libLLVM.so.<ver>
//! lib/clang/<ver>/include/   ← cc-rs C/C++ headers
//! ```
//!
//! Why two LLVM modules?
//!   * `llvm.rs` — MSVC-specific, version-pinned at 21.1.5, ships
//!     pre-compressed `.tar.zst` per-host from
//!     `clang-tool-chain-bins`. It pre-dates the `soldr-toolchain`
//!     Phase A pipeline.
//!   * `llvm_tools_bundle.rs` (this file) — the soldr#997 Phase A
//!     bundle. Comes from soldr-toolchain's forge-built recipes; one
//!     row per cross-compile-driver host (today: linux-x64 only;
//!     linux-arm64 / macos-arm64 hosts to follow when soldr's
//!     bootstrap supports them as drivers).
//!
//! Eventually `llvm.rs` and `llvm_tools_bundle.rs` should consolidate
//! into a single module, but that needs a catalogue migration. Keep
//! them separate until soldr#997 closes and the xwin lane proves the
//! new pipeline is healthy.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

/// LLVM version the soldr-toolchain `recipes/llvm-tools-linux-x64/`
/// recipe pins. Must match `LLVM_VERSION_DEFAULT` in the recipe file.
pub const MANAGED_LLVM_TOOLS_VERSION: &str = "20.1.7";

/// Catalogue layout: cross-compile-driver host → recipe slug. Today
/// only linux x86_64 is wired. Linux arm64 + macOS arm64 hosts can
/// be added when soldr's bootstrap matrix supports them as drivers
/// (today the docker image is linux x86_64 only — see soldr#997).
pub const LLVM_TOOLS_HOSTS: &[(&str, &str)] = &[("x86_64-unknown-linux-gnu", "linux-x64-gnu")];

/// Catalogue slug for a host triple.
pub fn host_slug_for(host_triple: &str) -> Option<&'static str> {
    LLVM_TOOLS_HOSTS
        .iter()
        .find(|(rust, _)| *rust == host_triple)
        .map(|(_, slug)| *slug)
}

/// Construct the expected `assets`-branch URL for the LLVM-tools
/// bundle. Catalogue layout: `llvm-tools/<version>/<slug>/bundle.tar.zst`.
pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("llvm-tools", version, slug)
}

/// Ensure the LLVM-tools bundle is materialized for the given driver
/// host. Returns the directory containing `bin/` + `lib/` + `include/`.
pub async fn ensure_llvm_tools_bundle(
    paths: &SoldrPaths,
    host_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = host_slug_for(host_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no llvm-tools bundle for host {host_triple}; \
             supported: {:?}",
            LLVM_TOOLS_HOSTS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(
        paths,
        "llvm-tools",
        MANAGED_LLVM_TOOLS_VERSION,
        slug,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_slug_for_known_triple() {
        assert_eq!(
            host_slug_for("x86_64-unknown-linux-gnu"),
            Some("linux-x64-gnu")
        );
        assert_eq!(host_slug_for("wasm32-unknown-unknown"), None);
    }

    #[test]
    fn asset_url_layout_matches_catalogue() {
        let u = asset_url_for(MANAGED_LLVM_TOOLS_VERSION, "linux-x64-gnu");
        assert!(u.starts_with("https://media.githubusercontent.com/media/"));
        assert!(u.contains("/llvm-tools/20.1.7/linux-x64-gnu/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    }

    #[test]
    fn version_constant_well_formed() {
        let parts: Vec<&str> = MANAGED_LLVM_TOOLS_VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "expected MAJOR.MINOR.PATCH");
        for p in parts {
            assert!(
                p.chars().all(|c| c.is_ascii_digit()),
                "non-digit in version: {p}"
            );
        }
    }
}
