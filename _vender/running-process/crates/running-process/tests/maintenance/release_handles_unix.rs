//! POSIX path: `release-handles` is a no-op that exits with an
//! informational message. See #230.

#![cfg(all(feature = "client", unix))]

use std::path::Path;

use running_process::maintenance::run_release_handles;

#[test]
fn release_handles_on_posix_is_a_successful_noop() {
    let outcome = run_release_handles(Path::new("/tmp/some-test-path")).expect("ok");
    assert!(
        outcome.already_clean,
        "POSIX should always report already_clean=true"
    );
    assert_eq!(outcome.manifests_scanned, 0);
    assert_eq!(outcome.handles_released, 0);
    assert!(
        outcome.message.contains("delete-on-close")
            || outcome.message.to_ascii_lowercase().contains("posix"),
        "message should explain the no-op rationale, got: {}",
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
fn release_handles_json_output_has_stable_keys() {
    let outcome = run_release_handles(Path::new("/tmp/some-test-path")).expect("ok");
    let json = outcome.to_json();
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
