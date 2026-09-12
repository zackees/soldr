//! Unit tests for [`super`].

use super::*;

#[test]
fn parse_accepts_both_tree_names() {
    assert_eq!(CookTree::parse("analysis").unwrap(), CookTree::Analysis);
    assert_eq!(CookTree::parse("tests").unwrap(), CookTree::Tests);
}

#[test]
fn parse_rejects_unknown_values_and_names_both_valid_ones() {
    for bad in ["analysis-tree", "", "TESTS"] {
        let error = CookTree::parse(bad).unwrap_err().to_string();
        assert!(error.contains("analysis"), "{error}");
        assert!(error.contains("tests"), "{error}");
    }
}

#[test]
fn default_tree_is_analysis() {
    assert_eq!(CookTree::default(), CookTree::Analysis);
}

#[test]
fn directory_and_operation_pairs() {
    assert_eq!(CookTree::Analysis.directory(), "target");
    assert_eq!(CookTree::Analysis.operation(), "check");
    assert_eq!(CookTree::Tests.directory(), "tests");
    assert_eq!(CookTree::Tests.operation(), "build");
}

#[test]
fn both_trees_use_the_ci_test_plans_channel_key() {
    let channel = "nightly-2026-05-28";
    let host = "x86_64-unknown-linux-gnu";
    // soldr#3049: every Dylint target tree is keyed by the host-qualified
    // toolchain, because that is the `RUSTUP_TOOLCHAIN` cargo-dylint and the
    // UI-test stages both name their directories by. The analysis tree used
    // the truncated driver-identity key instead and cooked a directory that
    // `soldr ci-test` never read.
    for tree in [CookTree::Analysis, CookTree::Tests] {
        assert_eq!(
            tree.channel_segment(channel, host),
            crate::ci_test::plan::canonical_channel(channel, host),
            "{tree:?}"
        );
        assert_eq!(
            tree.channel_segment(channel, host),
            "nightly-2026-05-28-x86_64-unknown-linux-gnu"
        );
    }
}
