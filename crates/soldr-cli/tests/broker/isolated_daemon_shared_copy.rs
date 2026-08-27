//! One daemon copy per volume, not one per test (soldr#2734).
//!
//! On the win-gnu target-run lane the workspace is `D:` and the test roots are
//! on `C:`. A hard link cannot cross volumes, so `isolated_daemon.rs`'s copy
//! fallback ran for *every* isolated-daemon test. Measured on that lane: the
//! workspace volume did not move while temp went 31.03 GiB -> 13.60 GiB, and
//! the shard died on `Os { code: 112, kind: StorageFull }`.
//!
//! The property under test is the one that makes that arithmetic go away: N
//! test roots sharing a parent must resolve to **one** staged copy, so the cost
//! is one binary plus N directory entries rather than N binaries.
//!
//! These call `shared_daemon_copy` directly rather than going through
//! `isolated_daemon_executable`. That function only reaches the shared path
//! when a direct hard link *fails*, which on a single-volume machine -- every
//! machine this suite runs on except that lane -- never happens. Testing
//! through it would exercise the direct link and assert nothing about the fix.

use std::path::Path;

use crate::common::isolated_daemon::shared_daemon_copy;

/// A parent directory holding several per-test roots, mirroring the real
/// layout: each test's `TempDir` is a sibling under one temp root.
fn parent_with_roots(count: usize) -> (tempfile::TempDir, Vec<std::path::PathBuf>) {
    let parent = tempfile::tempdir().expect("parent temp dir");
    let roots = (0..count)
        .map(|index| {
            let root = parent.path().join(format!("test-root-{index}"));
            std::fs::create_dir_all(&root).expect("create test root");
            root
        })
        .collect();
    (parent, roots)
}

fn write_source(directory: &Path, contents: &[u8]) -> std::path::PathBuf {
    let source = directory.join("soldr-daemon-source");
    std::fs::write(&source, contents).expect("write source");
    source
}

/// Count the staged binaries, ignoring the directory that holds them.
fn staged_files(parent: &Path) -> Vec<std::path::PathBuf> {
    let shared = parent.join("soldr-shared-test-daemon");
    let Ok(entries) = std::fs::read_dir(&shared) else {
        return Vec::new();
    };
    let mut found: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    found.sort();
    found
}

#[test]
fn many_test_roots_resolve_to_one_staged_copy() {
    let source_home = tempfile::tempdir().expect("source home");
    let source = write_source(source_home.path(), b"pretend daemon binary");
    let (parent, roots) = parent_with_roots(4);

    let staged: Vec<_> = roots
        .iter()
        .map(|root| shared_daemon_copy(&source, root).expect("stage shared copy"))
        .collect();

    assert!(
        staged.windows(2).all(|pair| pair[0] == pair[1]),
        "every root must resolve to the same staged copy, got {staged:?}"
    );
    assert_eq!(
        staged_files(parent.path()).len(),
        1,
        "four roots must leave exactly one staged binary on the volume"
    );
    assert_eq!(
        std::fs::read(&staged[0]).expect("read staged"),
        b"pretend daemon binary",
        "the staged copy must be the source's content"
    );
}

/// The staged copy is what every later test links against, so serving stale
/// content would run the wrong daemon everywhere at once -- a worse failure
/// than the disk exhaustion this replaces, because it would not announce
/// itself.
#[test]
fn a_rebuilt_source_is_not_served_from_the_old_staged_copy() {
    let source_home = tempfile::tempdir().expect("source home");
    let source = write_source(source_home.path(), b"first build");
    let (parent, roots) = parent_with_roots(1);

    let first = shared_daemon_copy(&source, &roots[0]).expect("stage first");
    assert_eq!(std::fs::read(&first).unwrap(), b"first build");

    // A rebuild: different length, and a later mtime.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&source, b"second build, longer").expect("rewrite source");

    let second = shared_daemon_copy(&source, &roots[0]).expect("stage second");
    assert_ne!(
        first, second,
        "a rebuilt source must not reuse the old name"
    );
    assert_eq!(
        std::fs::read(&second).unwrap(),
        b"second build, longer",
        "the staged copy must track the rebuilt source"
    );
    assert_eq!(
        staged_files(parent.path()).len(),
        2,
        "the old copy stays: concurrent tests may still hold links to it"
    );
}

/// Second call must reuse, not recopy. If it recopied, the fix would only move
/// the write amplification rather than remove it.
#[test]
fn a_second_call_reuses_the_published_copy() {
    let source_home = tempfile::tempdir().expect("source home");
    let source = write_source(source_home.path(), b"daemon");
    let (parent, roots) = parent_with_roots(1);

    let first = shared_daemon_copy(&source, &roots[0]).expect("stage");
    let before = std::fs::metadata(&first).expect("stat").modified().ok();

    std::thread::sleep(std::time::Duration::from_millis(20));
    let second = shared_daemon_copy(&source, &roots[0]).expect("stage again");

    assert_eq!(first, second);
    assert_eq!(
        std::fs::metadata(&second).expect("stat").modified().ok(),
        before,
        "the staged file must not have been rewritten on the second call"
    );
    assert_eq!(staged_files(parent.path()).len(), 1);
}

/// The staged copy is deliberately a sibling of the roots rather than inside
/// one. Every root is a `TempDir` that its test drops, so a copy staged inside
/// one would be deleted out from under the tests still linking to it.
#[test]
fn the_staged_copy_lives_outside_every_test_root() {
    let source_home = tempfile::tempdir().expect("source home");
    let source = write_source(source_home.path(), b"daemon");
    let (_parent, roots) = parent_with_roots(2);

    let staged = shared_daemon_copy(&source, &roots[0]).expect("stage");
    for root in &roots {
        assert!(
            !staged.starts_with(root),
            "staged copy {staged:?} must not live inside test root {root:?}"
        );
    }
}

/// Leaves no `pending-*` file behind on the happy path -- those are the
/// pre-rename staging files, and one left per invocation would reproduce the
/// per-test write amplification under a different name.
#[test]
fn publishing_leaves_no_pending_files() {
    let source_home = tempfile::tempdir().expect("source home");
    let source = write_source(source_home.path(), b"daemon");
    let (parent, roots) = parent_with_roots(3);
    for root in &roots {
        shared_daemon_copy(&source, root).expect("stage");
    }
    let pending: Vec<_> = staged_files(parent.path())
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("pending-"))
        })
        .collect();
    assert!(pending.is_empty(), "pending files left behind: {pending:?}");
}

/// A missing source is a caller bug, but it must degrade to `None` so the
/// caller falls back to its own copy path rather than panicking inside a
/// caching optimisation.
#[test]
fn a_missing_source_yields_none_rather_than_panicking() {
    let source_home = tempfile::tempdir().expect("source home");
    let (_parent, roots) = parent_with_roots(1);
    let absent = source_home.path().join("no-such-daemon");
    assert!(shared_daemon_copy(&absent, &roots[0]).is_none());
}
