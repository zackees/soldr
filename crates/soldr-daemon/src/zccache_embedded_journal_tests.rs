//! Compile-journal migration tests for `zccache_embedded.rs`.
//!
//! Split into their own file so `zccache_embedded.rs` stays under the
//! 1,000-line production ceiling enforced by `.github/scripts/loc_ceiling.py`.

use super::*;

#[test]
fn startup_scrubs_live_and_rotated_pre_redaction_journals() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/zccache/compile_journal_env_security_v1.json"
    ))
    .unwrap();
    let legacy = serde_json::to_string(&fixture["legacy_record"]).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let current = embedded_compile_journal_path(&paths);
    let rotated = current.with_file_name("compile_journal.jsonl.123");
    std::fs::create_dir_all(current.parent().unwrap()).unwrap();
    for path in [&current, &rotated] {
        std::fs::write(path, format!("{legacy}\nnot-json-with-secret\n")).unwrap();
    }

    scrub_existing_compile_journals(&paths).unwrap();

    for path in [&current, &rotated] {
        let body = std::fs::read_to_string(path).unwrap();
        assert!(!body.contains("legacy-full-env-token"));
        assert!(!body.contains("UNRESTRICTED_LEGACY_VARIABLE"));
        assert!(!body.contains("not-json-with-secret"));
        let row: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert!(row.get("env").is_none());
    }
    assert!(current.starts_with(paths.cache.join("zccache/daemon-state/embedded-v1")));
}

/// soldr#3174: the migration must run once per store, not once per daemon
/// start. It was 73.6s of a 77.6s cold start on the gate because it
/// re-parsed and rewrote every journal line every time.
#[test]
fn a_second_start_skips_the_scrub_entirely() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let current = embedded_compile_journal_path(&paths);
    std::fs::create_dir_all(current.parent().unwrap()).unwrap();
    std::fs::write(&current, "{\"msg\":\"clean\"}\n").unwrap();

    scrub_existing_compile_journals(&paths).unwrap();

    let marker = current.parent().unwrap().join(JOURNAL_SCRUB_MARKER);
    assert!(marker.exists(), "the first run must record the marker");

    // Plant something the scrub WOULD rewrite. If it runs again, this file
    // changes; if the marker works, it is left exactly as written.
    let dirty = "{\"env\":[[\"SECRET\",\"value\"]]}\nnot-json\n";
    std::fs::write(&current, dirty).unwrap();

    scrub_existing_compile_journals(&paths).unwrap();

    assert_eq!(
        std::fs::read_to_string(&current).unwrap(),
        dirty,
        "a marked store must not be re-scrubbed -- this is the 73.6s"
    );
}

/// The marker is per logs directory, so a store that has never been
/// scrubbed still gets its one pass even if some other store has a marker.
#[test]
fn an_unmarked_store_is_still_scrubbed() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let current = embedded_compile_journal_path(&paths);
    std::fs::create_dir_all(current.parent().unwrap()).unwrap();
    std::fs::write(&current, "{\"env\":[[\"SECRET\",\"leak\"]]}\n").unwrap();

    scrub_existing_compile_journals(&paths).unwrap();

    let body = std::fs::read_to_string(&current).unwrap();
    assert!(!body.contains("leak"), "an unmarked store must be scrubbed");
}

/// The marker lives among the journals but must not be mistaken for one.
#[test]
fn the_marker_is_not_itself_treated_as_a_journal() {
    assert!(!JOURNAL_SCRUB_MARKER.starts_with("compile_journal.jsonl"));
}

/// An unwritable marker degrades to the old behaviour -- slow, but correct
/// -- rather than failing daemon startup.
#[test]
fn a_marker_that_cannot_be_written_does_not_fail_startup() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let current = embedded_compile_journal_path(&paths);
    let logs = current.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(&current, "{\"msg\":\"clean\"}\n").unwrap();
    // A directory where the marker file wants to be: the write fails.
    std::fs::create_dir_all(logs.join(JOURNAL_SCRUB_MARKER)).unwrap();

    scrub_existing_compile_journals(&paths).expect("startup must survive");
}
