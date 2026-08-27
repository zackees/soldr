//! Single source of truth for soldr's canonical 9-target list (#941, #2336).
//!
//! These are the Rust target triples soldr ships full cross-compile
//! support for:
//!
//! - Linux x86_64-musl / aarch64-musl / x86_64-gnu / aarch64-gnu
//! - macOS x86_64 / aarch64
//! - Windows MSVC x86_64 / aarch64 and Windows GNU x86_64
//!
//! 32-bit triples (i686-*, armv7-*) and tier-2/tier-3 targets
//! (freebsd, wasm32, android, …) are deliberately omitted from this
//! list. They can still be passed explicitly via `--target <triple>`;
//! they just aren't covered by the catalogue-backed asset bundles
//! soldr#997 expands.
//!
//! ## Why this lives in `crate::core`
//!
//! Three call sites need the same constant — `--target all` fallback
//! (soldr#937), the `examples/docker-cross-all/` recipe scripts, and
//! the future `soldr targets` discovery subcommand. Previously the
//! list lived in `[workspace.metadata.soldr].targets` in the root
//! `Cargo.toml`; reading that requires `cargo metadata` which is
//! unavailable in containerized image-build contexts. Promoting the
//! list to a compile-time constant in `core` lets every soldr call
//! site reach it without runtime metadata.
//!
//! The workspace metadata block is **kept** (`Cargo.toml`
//! `[workspace.metadata.soldr]`) as the project-pinned form. A unit
//! test in `crates/soldr-cli/tests/guards/canonical_targets_parity.rs`
//! asserts the two lists agree byte-for-byte at build time so they
//! cannot drift.

/// Soldr's canonical 9-target list. Order is stable — callers may
/// iterate it deterministically.
pub const CANONICAL_TARGETS: &[&str] = &[
    "x86_64-pc-windows-msvc",
    "x86_64-pc-windows-gnu",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
];

/// Borrowed accessor — the canonical 9-target list as `&'static [&'static str]`.
/// Use this from any soldr call site that today queries
/// `[workspace.metadata.soldr].targets` so the same byte-identical
/// list is returned in every context (with or without a workspace
/// `Cargo.toml` on disk).
pub fn canonical_targets() -> &'static [&'static str] {
    CANONICAL_TARGETS
}

/// Returns `true` iff `triple` is in [`CANONICAL_TARGETS`].
/// Cheap linear scan over 9 entries.
pub fn is_canonical(triple: &str) -> bool {
    CANONICAL_TARGETS.contains(&triple)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_targets_length_is_9() {
        assert_eq!(CANONICAL_TARGETS.len(), 9);
    }

    #[test]
    fn canonical_targets_are_all_recognized_triples() {
        // Every entry must parse as a Rust target triple — guards
        // against typos like accidentally dropping a hyphen.
        // Apple triples have 2 hyphens (arch-apple-darwin); the
        // others have 3 (arch-vendor-os-env). Both are valid.
        for triple in CANONICAL_TARGETS {
            assert!(
                triple.matches('-').count() >= 2,
                "{triple} is missing components — should be `<arch>-<vendor>-<os>[-<env>]`",
            );
            assert!(!triple.contains(' '), "{triple} contains whitespace",);
        }
    }

    #[test]
    fn is_canonical_matches_listing() {
        for t in CANONICAL_TARGETS {
            assert!(is_canonical(t), "{t} not recognized by is_canonical");
        }
        assert!(
            !is_canonical("i686-pc-windows-msvc"),
            "32-bit shouldn't be canonical"
        );
        assert!(
            !is_canonical("wasm32-unknown-unknown"),
            "wasm not canonical"
        );
        assert!(!is_canonical(""), "empty string");
    }
}
