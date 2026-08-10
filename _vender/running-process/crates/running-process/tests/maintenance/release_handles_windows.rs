//! Windows path: `release-handles` Phase 1 is a successful stub that
//! returns "no manifests to scan yet". See #230.

#![cfg(all(feature = "client", windows))]

use std::path::Path;

use running_process::maintenance::run_release_handles;

#[test]
fn release_handles_phase1_stub_returns_no_manifests_message() {
    let outcome = run_release_handles(Path::new(r"C:\Users\example\worktree")).expect("ok");
    assert!(
        outcome.already_clean,
        "Phase 1 stub should always report already_clean=true"
    );
    assert_eq!(outcome.manifests_scanned, 0);
    assert_eq!(outcome.handles_released, 0);
    assert!(
        outcome.message.to_lowercase().contains("phase 2")
            || outcome.message.to_lowercase().contains("manifest"),
        "Phase 1 stub message should mention Phase 2 / manifest registry, got: {}",
        outcome.message
    );
}

#[test]
fn release_handles_rejects_empty_path() {
    let err = run_release_handles(Path::new("")).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("non-empty") || msg.contains("empty"),
        "error message should mention empty: {msg}"
    );
}

#[test]
fn release_handles_json_output_is_well_formed() {
    let outcome = run_release_handles(Path::new(r"C:\Users\example\worktree")).expect("ok");
    let json = outcome.to_json();
    // Backslashes must be escaped in JSON strings.
    assert!(json.contains("C:\\\\Users\\\\example\\\\worktree"));
    for key in [
        "\"path\":",
        "\"manifests_scanned\":",
        "\"handles_released\":",
        "\"already_clean\":",
        "\"message\":",
    ] {
        assert!(json.contains(key), "missing key {key} in JSON {json}");
    }
}
