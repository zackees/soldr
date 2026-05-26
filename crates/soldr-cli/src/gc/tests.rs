//! Unit tests for [`crate::gc`].

use super::*;

#[test]
fn gc_purge_prompt_defaults_enter_to_yes() {
    for input in ["", "\n", "y", "Y", "yes", " YES "] {
        assert!(parse_gc_purge_answer(input), "expected {input:?} to accept");
    }
    for input in ["n", "no", "anything else"] {
        assert!(!parse_gc_purge_answer(input), "expected {input:?} to skip");
    }
}

#[test]
fn gc_purge_worker_count_is_bounded() {
    assert_eq!(gc_purge_worker_count_for(0), 1);
    assert_eq!(gc_purge_worker_count_for(1), 1);
    assert_eq!(gc_purge_worker_count_for(2), 2);
    assert_eq!(gc_purge_worker_count_for(16), 4);
}

// -------------------------------------------------------------------
// last-used resolution for cargo_registry_src entries (issue #349).
// The precedence rule is: prefer cargo's `.global-cache` SQLite row
// when one exists for `(registry, crate, version)`; otherwise fall
// back to the directory's filesystem mtime.
// -------------------------------------------------------------------

fn fake_metadata_with_mtime(temp_path: &std::path::Path, mtime_unix: u64) -> std::fs::Metadata {
    // Construct a synthetic file with a controlled mtime so the test
    // exercises real `Metadata::modified()` rather than mocking the
    // OS layer. `filetime::set_file_mtime` is in dev-deps.
    std::fs::write(temp_path, b"x").unwrap();
    let ft = filetime::FileTime::from_unix_time(mtime_unix as i64, 0);
    filetime::set_file_mtime(temp_path, ft).unwrap();
    std::fs::metadata(temp_path).unwrap()
}

#[test]
fn last_used_prefers_global_cache_when_key_present() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("probe");
    let meta = fake_metadata_with_mtime(&f, 1_700_000_000);
    let mut map: std::collections::HashMap<
        crate::cache_lib::cargo_global_cache::RegistrySrcKey,
        i64,
    > = std::collections::HashMap::new();
    map.insert(
        (
            "index.crates.io-abc123".to_string(),
            "serde".to_string(),
            "1.0.0".to_string(),
        ),
        1_800_000_000,
    );
    let (ts, source) =
        resolve_registry_src_last_used(Some(&map), "index.crates.io-abc123", "serde-1.0.0", &meta);
    assert_eq!(ts, 1_800_000_000);
    assert_eq!(source, "global_cache");
}

#[test]
fn last_used_falls_back_to_mtime_when_tracker_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("probe");
    let meta = fake_metadata_with_mtime(&f, 1_700_000_000);
    let (ts, source) =
        resolve_registry_src_last_used(None, "index.crates.io-abc123", "serde-1.0.0", &meta);
    assert_eq!(ts, 1_700_000_000);
    assert_eq!(source, "fs_mtime");
}

#[test]
fn last_used_falls_back_to_mtime_when_key_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("probe");
    let meta = fake_metadata_with_mtime(&f, 1_700_000_000);
    // Tracker exists (Some), but doesn't have a row for this crate.
    // The fallback must be per-crate, not per-tracker.
    let map: std::collections::HashMap<
        crate::cache_lib::cargo_global_cache::RegistrySrcKey,
        i64,
    > = std::collections::HashMap::new();
    let (ts, source) = resolve_registry_src_last_used(
        Some(&map),
        "index.crates.io-abc123",
        "serde-1.0.0",
        &meta,
    );
    assert_eq!(ts, 1_700_000_000);
    assert_eq!(source, "fs_mtime");
}

#[test]
fn last_used_falls_back_when_dir_name_is_not_versioned() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("probe");
    let meta = fake_metadata_with_mtime(&f, 1_700_000_000);
    // `split_dir_name` returns None for names that don't match the
    // `<crate>-<digit-prefixed-version>` shape. The walker must then
    // fall back to mtime rather than crashing or returning 0.
    let map: std::collections::HashMap<
        crate::cache_lib::cargo_global_cache::RegistrySrcKey,
        i64,
    > = std::collections::HashMap::new();
    let (ts, source) =
        resolve_registry_src_last_used(Some(&map), "index.crates.io-abc123", "bare-serde", &meta);
    assert_eq!(ts, 1_700_000_000);
    assert_eq!(source, "fs_mtime");
}

#[test]
fn split_dir_name_recovers_crate_and_version() {
    assert_eq!(split_dir_name("serde-1.0.0"), Some(("serde", "1.0.0")));
    assert_eq!(split_dir_name("syn-2.0.16"), Some(("syn", "2.0.16")));
    // Hyphenated crate names with a numeric-suffix version still pick
    // the last digit-prefixed hyphen.
    assert_eq!(
        split_dir_name("aws-lc-sys-0.21.1"),
        Some(("aws-lc-sys", "0.21.1"))
    );
    // No digit-prefixed hyphen → None.
    assert_eq!(split_dir_name("serde"), None);
    assert_eq!(split_dir_name("foo-bar"), None);
}
