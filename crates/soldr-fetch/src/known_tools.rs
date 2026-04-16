//! Registry of ecosystem tools with known GitHub Releases and cargo-subcommand mapping.
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
    /// Name used as `cargo <sub>`.
    pub cargo_subcommand: &'static str,
    /// Binary shipped inside the release archive (no OS extension).
    pub binary_name: &'static str,
    /// Optional (owner, repo) override; skips the crates.io lookup when set.
    pub repo: Option<(&'static str, &'static str)>,
    /// Optional release-tag prefix used to filter monorepo releases, e.g.
    /// `"cargo-audit/"` to pick only `cargo-audit/v0.21.0`-style tags.
    pub tag_prefix: Option<&'static str>,
}

pub const KNOWN_TOOLS: &[ToolSpec] = &[
    ToolSpec {
        crate_name: "cargo-nextest",
        cargo_subcommand: "nextest",
        binary_name: "cargo-nextest",
        repo: Some(("nextest-rs", "nextest")),
        tag_prefix: Some("cargo-nextest-"),
    },
    ToolSpec {
        crate_name: "cargo-deny",
        cargo_subcommand: "deny",
        binary_name: "cargo-deny",
        repo: Some(("EmbarkStudios", "cargo-deny")),
        tag_prefix: None,
    },
    ToolSpec {
        crate_name: "cargo-audit",
        cargo_subcommand: "audit",
        binary_name: "cargo-audit",
        repo: Some(("rustsec", "rustsec")),
        tag_prefix: Some("cargo-audit/"),
    },
    ToolSpec {
        crate_name: "cargo-llvm-cov",
        cargo_subcommand: "llvm-cov",
        binary_name: "cargo-llvm-cov",
        repo: Some(("taiki-e", "cargo-llvm-cov")),
        tag_prefix: None,
    },
];

pub fn lookup_by_crate(crate_name: &str) -> Option<&'static ToolSpec> {
    KNOWN_TOOLS.iter().find(|t| t.crate_name == crate_name)
}

pub fn lookup_by_cargo_subcommand(sub: &str) -> Option<&'static ToolSpec> {
    KNOWN_TOOLS.iter().find(|t| t.cargo_subcommand == sub)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_by_crate_finds_registered_tools() {
        assert_eq!(
            lookup_by_crate("cargo-nextest").unwrap().cargo_subcommand,
            "nextest"
        );
        assert_eq!(
            lookup_by_crate("cargo-llvm-cov").unwrap().binary_name,
            "cargo-llvm-cov"
        );
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
                assert_ne!(
                    a.cargo_subcommand, b.cargo_subcommand,
                    "duplicate cargo_subcommand in registry"
                );
            }
        }
    }
}
