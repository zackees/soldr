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
}

pub const KNOWN_TOOLS: &[ToolSpec] = &[
    // Phase 2 — test + security.
    ToolSpec {
        crate_name: "cargo-nextest",
        cargo_subcommand: Some("nextest"),
        binary_name: "cargo-nextest",
        repo: Some(("nextest-rs", "nextest")),
        tag_prefix: Some("cargo-nextest-"),
    },
    ToolSpec {
        crate_name: "cargo-deny",
        cargo_subcommand: Some("deny"),
        binary_name: "cargo-deny",
        repo: Some(("EmbarkStudios", "cargo-deny")),
        tag_prefix: None,
    },
    ToolSpec {
        crate_name: "cargo-audit",
        cargo_subcommand: Some("audit"),
        binary_name: "cargo-audit",
        repo: Some(("rustsec", "rustsec")),
        tag_prefix: Some("cargo-audit/"),
    },
    ToolSpec {
        crate_name: "cargo-llvm-cov",
        cargo_subcommand: Some("llvm-cov"),
        binary_name: "cargo-llvm-cov",
        repo: Some(("taiki-e", "cargo-llvm-cov")),
        tag_prefix: None,
    },
    // Phase 3 — dev ergonomics.
    ToolSpec {
        crate_name: "cargo-udeps",
        cargo_subcommand: Some("udeps"),
        binary_name: "cargo-udeps",
        repo: Some(("est31", "cargo-udeps")),
        tag_prefix: None,
    },
    ToolSpec {
        crate_name: "cargo-semver-checks",
        cargo_subcommand: Some("semver-checks"),
        binary_name: "cargo-semver-checks",
        repo: Some(("obi1kenobi", "cargo-semver-checks")),
        tag_prefix: None,
    },
    ToolSpec {
        crate_name: "cargo-expand",
        cargo_subcommand: Some("expand"),
        binary_name: "cargo-expand",
        repo: Some(("dtolnay", "cargo-expand")),
        tag_prefix: None,
    },
    ToolSpec {
        crate_name: "cargo-watch",
        cargo_subcommand: Some("watch"),
        binary_name: "cargo-watch",
        repo: Some(("watchexec", "cargo-watch")),
        tag_prefix: None,
    },
    // Phase 4 — build + docs. None of these are cargo subcommands — they are
    // top-level tools invoked as `soldr cross ...`, `soldr mdbook ...`, etc.
    ToolSpec {
        crate_name: "cross",
        cargo_subcommand: None,
        binary_name: "cross",
        repo: Some(("cross-rs", "cross")),
        tag_prefix: None,
    },
    ToolSpec {
        crate_name: "mdbook",
        cargo_subcommand: None,
        binary_name: "mdbook",
        repo: Some(("rust-lang", "mdBook")),
        tag_prefix: None,
    },
    ToolSpec {
        crate_name: "cbindgen",
        cargo_subcommand: None,
        binary_name: "cbindgen",
        repo: Some(("mozilla", "cbindgen")),
        tag_prefix: None,
    },
];

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
    fn top_level_tools_are_registered_without_cargo_subcommand() {
        for crate_name in ["cross", "mdbook", "cbindgen"] {
            let spec = lookup_by_crate(crate_name)
                .unwrap_or_else(|| panic!("missing registry entry for {crate_name}"));
            assert_eq!(spec.cargo_subcommand, None);
            assert!(spec.repo.is_some());
        }
    }
}
