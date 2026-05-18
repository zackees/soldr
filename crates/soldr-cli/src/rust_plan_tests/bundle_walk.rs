//! Tests for the `SOLDR_TARGET_CACHE_TAR_THREADS` parser and the bundle
//! file-walk that backs the thin-slice manifest. See issue #272 for the
//! determinism contract these tests pin down.

use crate::rust_plan::{
    parse_rust_artifact_cache_tar_threads, resolve_bundle_walk_thread_count, walk_bundle_files,
    BUNDLE_WALK_THREAD_CAP,
};

#[test]
fn tar_threads_unset_or_blank_yields_none() {
    assert!(parse_rust_artifact_cache_tar_threads("").unwrap().is_none());
    assert!(parse_rust_artifact_cache_tar_threads("   ")
        .unwrap()
        .is_none());
}

#[test]
fn tar_threads_auto_is_normalized_lowercase() {
    assert_eq!(
        parse_rust_artifact_cache_tar_threads("auto").unwrap(),
        Some("auto".to_string())
    );
    assert_eq!(
        parse_rust_artifact_cache_tar_threads("  AUTO ").unwrap(),
        Some("auto".to_string())
    );
}

#[test]
fn tar_threads_positive_integer_passes_through() {
    for raw in ["1", "4", "8", "16"] {
        assert_eq!(
            parse_rust_artifact_cache_tar_threads(raw).unwrap(),
            Some(raw.to_string())
        );
    }
}

#[test]
fn tar_threads_rejects_zero_negative_and_garbage() {
    for raw in ["0", "-1", "1.5", "twelve", "auto4", "4 threads"] {
        let err = parse_rust_artifact_cache_tar_threads(raw)
            .expect_err(&format!("expected error for {raw:?}"));
        let msg = err.to_string();
        assert!(
            msg.contains("SOLDR_TARGET_CACHE_TAR_THREADS"),
            "error for {raw:?} must mention the env var, got {msg}"
        );
    }
}

/// Unset / `auto` / case-variants of `auto` must all yield `None`, which
/// signals "use rayon's global thread pool" to `walk_bundle_files`.
#[test]
fn bundle_walk_thread_count_auto_yields_none() {
    for raw in ["", "  ", "auto", "AUTO", " Auto "] {
        assert_eq!(
            resolve_bundle_walk_thread_count(raw).unwrap(),
            None,
            "raw {raw:?} should resolve to None (auto)"
        );
    }
}

/// An explicit `1` must turn into `Some(1)` so the walk takes the
/// sequential fallback path (no rayon overhead).
#[test]
fn bundle_walk_thread_count_one_forces_sequential() {
    assert_eq!(resolve_bundle_walk_thread_count("1").unwrap(), Some(1));
}

/// In-range explicit counts pass through unmodified; values above the
/// internal cap are clamped down to `BUNDLE_WALK_THREAD_CAP`.
#[test]
fn bundle_walk_thread_count_clamps_to_cap() {
    assert_eq!(resolve_bundle_walk_thread_count("2").unwrap(), Some(2));
    assert_eq!(
        resolve_bundle_walk_thread_count("8").unwrap(),
        Some(BUNDLE_WALK_THREAD_CAP)
    );
    // 64 → capped at BUNDLE_WALK_THREAD_CAP.
    assert_eq!(
        resolve_bundle_walk_thread_count("64").unwrap(),
        Some(BUNDLE_WALK_THREAD_CAP)
    );
    assert_eq!(
        resolve_bundle_walk_thread_count("9999").unwrap(),
        Some(BUNDLE_WALK_THREAD_CAP)
    );
}

/// Garbage values inherited from the parser must still propagate as
/// errors here so callers on the bare `RUSTC_WRAPPER` passthrough path
/// (which bypasses the cargo front-door validation) get a clear message
/// instead of a silent default.
#[test]
fn bundle_walk_thread_count_rejects_garbage() {
    for raw in ["0", "twelve", "1.5"] {
        let err = resolve_bundle_walk_thread_count(raw)
            .expect_err(&format!("expected error for {raw:?}"));
        assert!(
            err.to_string().contains("SOLDR_TARGET_CACHE_TAR_THREADS"),
            "error must reference the env var name"
        );
    }
}

/// Build a bundle layout with a handful of files at varying depths and
/// verify that the walker returns one entry per regular file with the
/// correct relative path string (forward-slashed, root-relative).
fn populate_walk_bundle_fixture(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("debug/deps")).unwrap();
    std::fs::create_dir_all(root.join("debug/build")).unwrap();
    std::fs::write(root.join("debug/deps/a.rlib"), b"alpha").unwrap();
    std::fs::write(root.join("debug/deps/b.rmeta"), b"beta!!").unwrap();
    std::fs::write(root.join("debug/build/c.txt"), b"gamma").unwrap();
    std::fs::write(root.join("top.txt"), b"delta-delta").unwrap();
}

/// The sequential path (`Some(1)`) must enumerate every file with the
/// expected relative paths and sizes. This is the baseline against which
/// the parallel walks are compared for determinism.
#[test]
fn walk_bundle_files_sequential_lists_every_file_with_size() {
    let bundle = tempfile::tempdir().expect("tempdir");
    populate_walk_bundle_fixture(bundle.path());

    let mut entries =
        walk_bundle_files(bundle.path(), Some(1)).expect("sequential walk must succeed");
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let observed: Vec<_> = entries
        .iter()
        .map(|e| (e.path.as_str(), e.size_bytes))
        .collect();
    assert_eq!(
        observed,
        vec![
            ("debug/build/c.txt", Some(5)),
            ("debug/deps/a.rlib", Some(5)),
            ("debug/deps/b.rmeta", Some(6)),
            ("top.txt", Some(11)),
        ]
    );
}

/// Output of the walk must be byte-identical (after the caller's
/// canonical sort) regardless of whether the metadata phase ran
/// sequentially, on rayon's global pool, or on a scoped explicit pool.
/// This is the determinism acceptance criterion from issue #272.
#[test]
fn walk_bundle_files_parallel_matches_sequential_after_sort() {
    let bundle = tempfile::tempdir().expect("tempdir");
    populate_walk_bundle_fixture(bundle.path());

    let mut sequential =
        walk_bundle_files(bundle.path(), Some(1)).expect("sequential walk must succeed");
    sequential.sort_by(|a, b| a.path.cmp(&b.path));

    for thread_count in [None, Some(2), Some(BUNDLE_WALK_THREAD_CAP)] {
        let mut parallel = walk_bundle_files(bundle.path(), thread_count)
            .unwrap_or_else(|e| panic!("walk failed with thread_count {thread_count:?}: {e}"));
        parallel.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            parallel, sequential,
            "thread_count {thread_count:?} produced a different file list after canonical sort"
        );
    }
}

/// A missing root is not an error — the bundle may legitimately not
/// exist yet (e.g. zccache restore produced nothing). The walk must
/// return an empty vec rather than propagating a `NotFound` IO error.
#[test]
fn walk_bundle_files_missing_root_returns_empty() {
    let bundle = tempfile::tempdir().expect("tempdir");
    let missing = bundle.path().join("never-created");
    for thread_count in [Some(1), None, Some(4)] {
        let entries = walk_bundle_files(&missing, thread_count)
            .unwrap_or_else(|e| panic!("missing root must not error ({thread_count:?}): {e}"));
        assert!(
            entries.is_empty(),
            "missing root walk with {thread_count:?} should be empty, got {entries:?}"
        );
    }
}
