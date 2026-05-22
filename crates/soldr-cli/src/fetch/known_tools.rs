//! Registry of ecosystem tools with known GitHub Releases and (optional) cargo-subcommand mapping.
//!
//! Some crates have monorepo-style release tags (e.g. `cargo-audit/v0.21.0`) or
//! live in a repository whose name differs from the crate. Falling back to the
//! crates.io → GitHub repository lookup plus `/releases/latest` can pick up the
//! wrong release in those cases. When a tool needs per-release handling, encode
//! it here once and fetch paths can use it directly.

#[derive(Debug, Clone, Copy)]
pub struct ToolSpec {
    /// crates.io crate name and fetch cache key.
    pub crate_name: &'static str,
    /// Name used as `cargo <sub>`. `None` for tools that are not cargo
    /// subcommands (e.g. `cross`, `mdbook`, `sccache`).
    pub cargo_subcommand: Option<&'static str>,
    /// Binary shipped inside the release archive (no OS extension).
    pub binary_name: &'static str,
    /// Optional (owner, repo) override; skips the crates.io lookup when set.
    pub repo: Option<(&'static str, &'static str)>,
    /// Optional release-tag prefix used to filter monorepo releases, e.g.
    /// `"cargo-audit/"` to pick only `cargo-audit/v0.21.0`-style tags.
    pub tag_prefix: Option<&'static str>,
    /// Optional pinned release version (without the leading `v`). When set,
    /// fetches resolve to exactly this version instead of the upstream
    /// `latest` release. Used for tools whose upstream release stream has
    /// drifted away from the platform coverage soldr depends on (e.g.
    /// `cargo-chef` stopped publishing Windows/macOS archives after v0.1.73).
    pub pinned_version: Option<&'static str>,
}

pub const KNOWN_TOOLS: &[ToolSpec] = &[
    // Phase 2 — test + security.
    ToolSpec {
        crate_name: "cargo-nextest",
        cargo_subcommand: Some("nextest"),
        binary_name: "cargo-nextest",
        repo: Some(("nextest-rs", "nextest")),
        tag_prefix: Some("cargo-nextest-"),
        pinned_version: None,
    },
    ToolSpec {
        crate_name: "cargo-deny",
        cargo_subcommand: Some("deny"),
        binary_name: "cargo-deny",
        repo: Some(("EmbarkStudios", "cargo-deny")),
        tag_prefix: None,
        pinned_version: None,
    },
    ToolSpec {
        crate_name: "cargo-audit",
        cargo_subcommand: Some("audit"),
        binary_name: "cargo-audit",
        repo: Some(("rustsec", "rustsec")),
        tag_prefix: Some("cargo-audit/"),
        pinned_version: None,
    },
    ToolSpec {
        crate_name: "cargo-llvm-cov",
        cargo_subcommand: Some("llvm-cov"),
        binary_name: "cargo-llvm-cov",
        repo: Some(("taiki-e", "cargo-llvm-cov")),
        tag_prefix: None,
        pinned_version: None,
    },
    // Phase 3 — dev ergonomics.
    ToolSpec {
        crate_name: "cargo-udeps",
        cargo_subcommand: Some("udeps"),
        binary_name: "cargo-udeps",
        repo: Some(("est31", "cargo-udeps")),
        tag_prefix: None,
        pinned_version: None,
    },
    ToolSpec {
        crate_name: "cargo-semver-checks",
        cargo_subcommand: Some("semver-checks"),
        binary_name: "cargo-semver-checks",
        repo: Some(("obi1kenobi", "cargo-semver-checks")),
        tag_prefix: None,
        pinned_version: None,
    },
    ToolSpec {
        crate_name: "cargo-expand",
        cargo_subcommand: Some("expand"),
        binary_name: "cargo-expand",
        repo: Some(("dtolnay", "cargo-expand")),
        tag_prefix: None,
        pinned_version: None,
    },
    ToolSpec {
        crate_name: "cargo-watch",
        cargo_subcommand: Some("watch"),
        binary_name: "cargo-watch",
        repo: Some(("watchexec", "cargo-watch")),
        tag_prefix: None,
        pinned_version: None,
    },
    // `cargo-chef` powers the `soldr cook` content-addressable dep-prebuild
    // (issue #359). Pinned to v0.1.73 — the most recent release that still
    // ships pre-built archives for Windows MSVC and macOS in addition to the
    // Linux assets the newer releases publish. Bumping the pin past v0.1.73
    // costs Windows/macOS coverage until upstream restores those targets.
    ToolSpec {
        crate_name: "cargo-chef",
        cargo_subcommand: Some("chef"),
        binary_name: "cargo-chef",
        repo: Some(("LukeMathWalker", "cargo-chef")),
        tag_prefix: None,
        pinned_version: Some(CARGO_CHEF_PINNED_VERSION),
    },
    // Phase 4 — build + docs. None of these are cargo subcommands — they are
    // top-level tools invoked as `soldr cross ...`, `soldr mdbook ...`, etc.
    ToolSpec {
        crate_name: "cross",
        cargo_subcommand: None,
        binary_name: "cross",
        repo: Some(("cross-rs", "cross")),
        tag_prefix: None,
        pinned_version: None,
    },
    ToolSpec {
        crate_name: "mdbook",
        cargo_subcommand: None,
        binary_name: "mdbook",
        repo: Some(("rust-lang", "mdBook")),
        tag_prefix: None,
        pinned_version: None,
    },
    ToolSpec {
        crate_name: "cbindgen",
        cargo_subcommand: None,
        binary_name: "cbindgen",
        repo: Some(("mozilla", "cbindgen")),
        tag_prefix: None,
        pinned_version: None,
    },
    // Phase 5 — web/wasm + cache. Top-level tools invoked directly.
    ToolSpec {
        crate_name: "wasm-pack",
        cargo_subcommand: None,
        binary_name: "wasm-pack",
        repo: Some(("rustwasm", "wasm-pack")),
        tag_prefix: None,
        pinned_version: None,
    },
    ToolSpec {
        crate_name: "trunk",
        cargo_subcommand: None,
        binary_name: "trunk",
        repo: Some(("trunk-rs", "trunk")),
        tag_prefix: None,
        pinned_version: None,
    },
    ToolSpec {
        crate_name: "sccache",
        cargo_subcommand: None,
        binary_name: "sccache",
        repo: Some(("mozilla", "sccache")),
        tag_prefix: None,
        pinned_version: None,
    },
    // Self-trampoline: `soldr --as <version>` fetches this entry so an older
    // soldr binary can handle the rest of the invocation.
    ToolSpec {
        crate_name: "soldr",
        cargo_subcommand: None,
        binary_name: "soldr",
        repo: Some(("zackees", "soldr")),
        tag_prefix: None,
        pinned_version: None,
    },
    // `crgx` is bundled into the combined soldr release archive
    // (release-auto.yml source-builds it from a pinned crgx tag), so
    // first-use needs no network round trip. The registry entry is the
    // fallback path when the bundle is unavailable (sideloaded soldr
    // binary, custom install, etc.) — it pins to the same upstream
    // version the bundle ships so `soldr crgx ...` is reproducible
    // across both paths. See SOLDR_CRGX_LOCAL_DIR for the runtime
    // override used by the npm shim and setup-soldr action.
    ToolSpec {
        crate_name: "crgx",
        cargo_subcommand: None,
        binary_name: "crgx",
        repo: Some(("yfedoseev", "crgx")),
        tag_prefix: None,
        pinned_version: Some(super::MANAGED_CRGX_VERSION),
    },
];

/// Pinned `cargo-chef` release that `soldr cook` resolves by default. See the
/// `cargo-chef` entry in [`KNOWN_TOOLS`] for the rationale (last release with
/// Windows MSVC + macOS prebuilds).
pub const CARGO_CHEF_PINNED_VERSION: &str = "0.1.73";

pub fn lookup_by_crate(crate_name: &str) -> Option<&'static ToolSpec> {
    KNOWN_TOOLS.iter().find(|t| t.crate_name == crate_name)
}

pub fn lookup_by_cargo_subcommand(sub: &str) -> Option<&'static ToolSpec> {
    KNOWN_TOOLS.iter().find(|t| t.cargo_subcommand == Some(sub))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_by_crate_finds_registered_tools() {
        assert_eq!(
            lookup_by_crate("cargo-nextest").unwrap().cargo_subcommand,
            Some("nextest")
        );
        assert_eq!(
            lookup_by_crate("cargo-llvm-cov").unwrap().binary_name,
            "cargo-llvm-cov"
        );
        assert_eq!(lookup_by_crate("mdbook").unwrap().cargo_subcommand, None);
        assert!(lookup_by_crate("not-a-tool").is_none());
    }

    #[test]
    fn lookup_by_cargo_subcommand_finds_registered_tools() {
        assert_eq!(
            lookup_by_cargo_subcommand("nextest").unwrap().crate_name,
            "cargo-nextest"
        );
        assert_eq!(
            lookup_by_cargo_subcommand("deny").unwrap().crate_name,
            "cargo-deny"
        );
        assert!(lookup_by_cargo_subcommand("build").is_none());
        // Tools with cargo_subcommand: None should never be returned here.
        assert!(lookup_by_cargo_subcommand("mdbook").is_none());
        assert!(lookup_by_cargo_subcommand("cross").is_none());
        assert!(lookup_by_cargo_subcommand("cbindgen").is_none());
    }

    #[test]
    fn cargo_audit_carries_monorepo_tag_prefix() {
        let spec = lookup_by_crate("cargo-audit").unwrap();
        assert_eq!(spec.tag_prefix, Some("cargo-audit/"));
    }

    #[test]
    fn known_tools_are_unique_by_crate_and_subcommand() {
        for (i, a) in KNOWN_TOOLS.iter().enumerate() {
            for b in KNOWN_TOOLS.iter().skip(i + 1) {
                assert_ne!(
                    a.crate_name, b.crate_name,
                    "duplicate crate_name in registry"
                );
                if let (Some(asub), Some(bsub)) = (a.cargo_subcommand, b.cargo_subcommand) {
                    assert_ne!(asub, bsub, "duplicate cargo_subcommand in registry");
                }
            }
        }
    }

    #[test]
    fn cargo_chef_is_registered_and_pinned() {
        // soldr cook (issue #359) is a shim around cargo-chef. Both the
        // registry entry and the pinned-version constant are part of the
        // public contract; if either changes, downstream users see a
        // different cargo-chef.
        let spec = lookup_by_crate("cargo-chef").expect("cargo-chef must be registered");
        assert_eq!(spec.cargo_subcommand, Some("chef"));
        assert_eq!(spec.binary_name, "cargo-chef");
        assert_eq!(spec.repo, Some(("LukeMathWalker", "cargo-chef")));
        assert_eq!(spec.pinned_version, Some(CARGO_CHEF_PINNED_VERSION));
        assert_eq!(CARGO_CHEF_PINNED_VERSION, "0.1.73");
        assert_eq!(
            lookup_by_cargo_subcommand("chef").map(|s| s.crate_name),
            Some("cargo-chef")
        );
    }

    #[test]
    fn crgx_is_registered_and_pinned_to_managed_version() {
        // soldr bundles crgx into the combined release archive (see
        // release-auto.yml's `Build crgx from pinned source` step).
        // The registry entry MUST pin to `MANAGED_CRGX_VERSION` so the
        // fallback fetch path resolves to the same version the bundle
        // ships. If these drift apart, `soldr crgx` from a sideloaded
        // binary lands on a different version than the bundled one —
        // exactly the cross-source inconsistency the pin is meant to
        // prevent.
        let spec = lookup_by_crate("crgx").expect("crgx must be registered");
        assert_eq!(spec.cargo_subcommand, None);
        assert_eq!(spec.binary_name, "crgx");
        assert_eq!(spec.repo, Some(("yfedoseev", "crgx")));
        assert_eq!(
            spec.pinned_version,
            Some(super::super::MANAGED_CRGX_VERSION)
        );
        assert_eq!(super::super::MANAGED_CRGX_VERSION, "0.1.0");
    }

    #[test]
    fn top_level_tools_are_registered_without_cargo_subcommand() {
        for crate_name in [
            "cross",
            "mdbook",
            "cbindgen",
            "wasm-pack",
            "trunk",
            "sccache",
        ] {
            let spec = lookup_by_crate(crate_name)
                .unwrap_or_else(|| panic!("missing registry entry for {crate_name}"));
            assert_eq!(spec.cargo_subcommand, None);
            assert!(spec.repo.is_some());
        }
    }
}
