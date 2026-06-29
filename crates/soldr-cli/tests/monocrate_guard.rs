//! Monocrate regression guard.
//!
//! soldr collapsed its four-crate workspace (soldr-core, soldr-fetch,
//! soldr-cache, soldr-cli) into a single `soldr-cli` crate. The
//! collapse buys us a single `Cargo.toml`, one dep list, no
//! cross-crate `From<SoldrError>` boilerplate, and trivial in-crate
//! visibility.
//!
//! It also costs us a structural invariant we want to keep: every
//! contributor (or PR-bot) adding a "small helper crate" silently
//! undoes the consolidation. This test fails the workspace `cargo
//! test` run if anyone re-introduces a second crate.
//!
//! Two independent checks, both must pass:
//!
//! 1. The workspace manifest's `members` list must contain exactly
//!    one entry, and that entry must be `crates/soldr-cli`.
//! 2. There must be exactly one `crates/*/Cargo.toml` on disk, so
//!    "added a crate dir but forgot to add it to members" is also
//!    rejected (cargo only warns about that, it doesn't fail).
//!
//! How to fix this if you tripped it:
//! - You probably did not mean to add a second crate. Fold the new
//!   module into `crates/soldr-cli/src/<your_module>/` instead.
//! - If you genuinely need to split (e.g. you are publishing a
//!   sub-library to crates.io): update the const in this file, the
//!   member list in `Cargo.toml`, and the architecture section of
//!   `CLAUDE.md`. All three should move together so the next
//!   contributor finds the new shape documented.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

/// The one and only workspace member. Update this list intentionally
/// — never just to make a failing test pass.
const ALLOWED_WORKSPACE_MEMBERS: &[&str] = &["crates/soldr-cli"];

#[test]
fn workspace_members_contains_exactly_the_allowed_set() {
    // soldr#1040 phase 2: source-tree-coupled.
    if common::should_skip_source_tree_test("workspace_members_contains_exactly_the_allowed_set") {
        return;
    }
    let root = workspace_root();
    let manifest_path = root.join("Cargo.toml");
    let raw = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
    let manifest: toml::Value =
        toml::from_str(&raw).unwrap_or_else(|e| panic!("parse workspace Cargo.toml: {e}"));

    let members = manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .unwrap_or_else(|| panic!("[workspace].members missing or not an array"));

    let actual: Vec<&str> = members.iter().filter_map(|v| v.as_str()).collect();

    let mut allowed_sorted: Vec<&str> = ALLOWED_WORKSPACE_MEMBERS.to_vec();
    allowed_sorted.sort_unstable();
    let mut actual_sorted = actual.clone();
    actual_sorted.sort_unstable();

    assert_eq!(
        actual_sorted,
        allowed_sorted,
        "\n\nMonocrate guard tripped: workspace.members in {} \
         does not match the allowed set.\n  allowed: {:?}\n  found:   {:?}\n\n\
         If you added a second crate on purpose, update \
         ALLOWED_WORKSPACE_MEMBERS in tests/monocrate_guard.rs and \
         the architecture section of CLAUDE.md to match.\n",
        manifest_path.display(),
        allowed_sorted,
        actual_sorted
    );
}

#[test]
fn exactly_one_cargo_toml_under_crates() {
    if common::should_skip_source_tree_test("exactly_one_cargo_toml_under_crates") {
        return;
    }
    let root = workspace_root();
    let crates_dir = root.join("crates");
    let entries = fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", crates_dir.display()));

    let mut found: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let cargo_toml = path.join("Cargo.toml");
        if cargo_toml.is_file() {
            found.push(cargo_toml);
        }
    }

    assert_eq!(
        found.len(),
        1,
        "\n\nMonocrate guard tripped: expected exactly one \
         crates/*/Cargo.toml on disk, found {}:\n  {}\n\n\
         The workspace.members check is fooled by a crate dir that \
         exists on disk but is omitted from members (cargo only \
         warns, it does not fail). This second check catches that \
         case. Fold the new crate into crates/soldr-cli/src/ as a \
         module instead.\n",
        found.len(),
        found
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    let only = found[0].parent().unwrap();
    let rel = only.strip_prefix(&root).unwrap_or(only);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    assert_eq!(
        rel_str, "crates/soldr-cli",
        "the single surviving crate must be soldr-cli; got {rel_str}"
    );
}

/// Walk up from this test's manifest dir until we find the workspace
/// root (the directory containing the `[workspace]` Cargo.toml).
fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut cur: &Path = manifest_dir.as_path();
    loop {
        let candidate = cur.join("Cargo.toml");
        if candidate.is_file() {
            let text = fs::read_to_string(&candidate).unwrap_or_default();
            if text.contains("[workspace]") {
                return cur.to_path_buf();
            }
        }
        cur = cur
            .parent()
            .unwrap_or_else(|| panic!("walked above filesystem root looking for workspace"));
    }
}
