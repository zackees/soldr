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
fn channel_segment_reuses_each_consumers_own_rule() {
    let channel = "nightly-2026-05-28";
    let host = "x86_64-unknown-linux-gnu";
    assert_eq!(
        CookTree::Analysis.channel_segment(channel, host),
        "nightly-2026-05-28"
    );
    // This is the whole point of the phase: the tests tree must land where
    // `ci_test/plan.rs`'s UI-test stages look for it, and that directory is
    // host-triple-suffixed while the analysis tree's is not (soldr#3042
    // FACT 1).
    assert_eq!(
        CookTree::Tests.channel_segment(channel, host),
        "nightly-2026-05-28-x86_64-unknown-linux-gnu"
    );
}
