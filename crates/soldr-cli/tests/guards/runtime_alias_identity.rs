//! soldr#3168: the test harness must not byte-compare a file with itself.
//!
//! `common::soldr_bin()` runs on essentially every integration test, and it
//! calls `materialize_runtime_alias`, whose fast path asks whether
//! `target/debug/soldr` and `target/debug/soldr-daemon` already hold the same
//! bytes. The alias is created with `hard_link`, so in the steady state they
//! are one inode -- and the question was being answered by reading 110 MB
//! twice, once per test process, because the memo that would skip it is
//! per-process and nextest gives every test its own.

use std::fs;

use crate::common;

/// The case that actually occurs in a warm `target/`: one inode, two names.
#[test]
#[cfg(unix)]
fn a_hardlinked_alias_is_recognised_without_reading_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let original = dir.path().join("soldr");
    let alias = dir.path().join("soldr-daemon");
    fs::write(&original, b"pretend this is 110 MB").expect("write");
    fs::hard_link(&original, &alias).expect("hard_link");

    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        fs::metadata(&original).expect("meta").ino(),
        fs::metadata(&alias).expect("meta").ino(),
        "fixture must actually be hardlinked or it proves nothing"
    );

    assert!(common::files_equal(&original, &alias));
}

/// The fallback the byte comparison was written for: genuinely distinct files
/// that happen to match. Must still be reported equal.
#[test]
fn two_distinct_files_with_identical_bytes_are_still_equal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let left = dir.path().join("soldr");
    let right = dir.path().join("soldr-daemon");
    // Larger than the 64 KiB read buffer, so the loop runs more than once.
    let body = vec![0xab_u8; 200 * 1024];
    fs::write(&left, &body).expect("write left");
    fs::write(&right, &body).expect("write right");

    assert!(common::files_equal(&left, &right));
}

/// The identity short-circuit must not swallow a real difference -- that would
/// skip a materialization the harness depends on.
#[test]
fn differing_files_are_not_equal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let left = dir.path().join("soldr");
    let right = dir.path().join("soldr-daemon");
    fs::write(&left, b"aaaa").expect("write left");
    fs::write(&right, b"bbbb").expect("write right");
    assert!(!common::files_equal(&left, &right));

    // Same length is the interesting case: length alone cannot separate these.
    let short = dir.path().join("short");
    fs::write(&short, b"aa").expect("write short");
    assert!(!common::files_equal(&left, &short));
}

/// A missing side is not equal to anything, and must not panic.
#[test]
fn a_missing_file_is_not_equal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let present = dir.path().join("soldr");
    fs::write(&present, b"x").expect("write");
    let absent = dir.path().join("does-not-exist");

    assert!(!common::files_equal(&present, &absent));
    assert!(!common::files_equal(&absent, &present));
}
