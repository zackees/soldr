//! Tests for the orphan-`.rmeta` pruner that defends against
//! soldr#410: when `cargo build` aborts mid-build after rustc has
//! emitted a crate's `.rmeta` but before the `.rlib` codegen pass
//! completes, the orphan `.rmeta` survives in
//! `target/<triple>/<profile>/deps/`. Subsequent builds then fail
//! with `E0463: can't find crate for X` because cargo passes
//! `--extern X=orphan.rmeta` and rustc cannot link an rmeta-only
//! crate.
//!
//! Pruner contract under test:
//!
//! 1. An `.rmeta` whose stem has a matching `.rlib`, `.so`,
//!    `.dylib`, or `.dll` sibling stays.
//! 2. An `.rmeta` with no matching companion is deleted.
//! 3. Non-`.rmeta` files are never touched.
//! 4. Files in subdirectories of `deps/` (e.g.,
//!    `deps/<somedir>/whatever.rmeta`) are never touched.
//! 5. A missing `deps/` directory is a no-op (returns 0).
//! 6. The walker reports the count of deleted files.
//!
//! The adversarial cases below pin down each leaf of the contract
//! plus interactions: per-crate-type companions (lib/cdylib/proc-
//! macro), hash-collision twins (two builds of the same crate at
//! different fingerprints), and subdir isolation.

use crate::rust_plan::{find_deps_dirs_for_test, prune_orphan_rmetas_in_deps};
use std::fs;
use std::path::Path;

/// Create an empty file at `dir/name`. Panics on IO failure — tests
/// run in an empty `tempdir()` so this is a programmer-error path.
fn touch(dir: &Path, name: &str) {
    fs::write(dir.join(name), b"").expect("touch");
}

fn exists(dir: &Path, name: &str) -> bool {
    dir.join(name).exists()
}

#[test]
fn missing_deps_dir_is_a_no_op() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nonexistent-deps");
    assert_eq!(prune_orphan_rmetas_in_deps(&missing), 0);
}

#[test]
fn empty_deps_dir_is_a_no_op() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(prune_orphan_rmetas_in_deps(tmp.path()), 0);
}

#[test]
fn paired_rmeta_and_rlib_are_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let deps = tmp.path();
    touch(deps, "libserde-2d377630294144c0.rmeta");
    touch(deps, "libserde-2d377630294144c0.rlib");
    touch(deps, "libserde-2d377630294144c0.d");

    assert_eq!(prune_orphan_rmetas_in_deps(deps), 0);
    assert!(exists(deps, "libserde-2d377630294144c0.rmeta"));
    assert!(exists(deps, "libserde-2d377630294144c0.rlib"));
    assert!(exists(deps, "libserde-2d377630294144c0.d"));
}

#[test]
fn orphan_rmeta_with_no_companion_is_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let deps = tmp.path();
    touch(deps, "libsoldr_core-bc09ee1008e7e294.rmeta");
    touch(deps, "libsoldr_core-bc09ee1008e7e294.d");

    assert_eq!(prune_orphan_rmetas_in_deps(deps), 1);
    assert!(!exists(deps, "libsoldr_core-bc09ee1008e7e294.rmeta"));
    // `.d` files are unrelated to the linking failure and must not be
    // touched: they're cargo dep-info bookkeeping, not artifacts.
    assert!(exists(deps, "libsoldr_core-bc09ee1008e7e294.d"));
}

#[test]
fn proc_macro_dll_on_windows_keeps_rmeta() {
    // Proc-macro crate types on Windows emit a `.dll` instead of an
    // `.rlib`. The rmeta-with-dll pair is a legitimate "built and
    // linkable" state and must not be pruned.
    let tmp = tempfile::tempdir().unwrap();
    let deps = tmp.path();
    touch(deps, "serde_derive-abc123.rmeta");
    touch(deps, "serde_derive-abc123.dll");

    assert_eq!(prune_orphan_rmetas_in_deps(deps), 0);
    assert!(exists(deps, "serde_derive-abc123.rmeta"));
    assert!(exists(deps, "serde_derive-abc123.dll"));
}

#[test]
fn proc_macro_so_on_linux_keeps_rmeta() {
    let tmp = tempfile::tempdir().unwrap();
    let deps = tmp.path();
    touch(deps, "libserde_derive-abc123.rmeta");
    touch(deps, "libserde_derive-abc123.so");

    assert_eq!(prune_orphan_rmetas_in_deps(deps), 0);
    assert!(exists(deps, "libserde_derive-abc123.rmeta"));
    assert!(exists(deps, "libserde_derive-abc123.so"));
}

#[test]
fn cdylib_dylib_on_macos_keeps_rmeta() {
    let tmp = tempfile::tempdir().unwrap();
    let deps = tmp.path();
    touch(deps, "libfoo-abc123.rmeta");
    touch(deps, "libfoo-abc123.dylib");

    assert_eq!(prune_orphan_rmetas_in_deps(deps), 0);
    assert!(exists(deps, "libfoo-abc123.rmeta"));
    assert!(exists(deps, "libfoo-abc123.dylib"));
}

#[test]
fn rlib_without_rmeta_is_untouched() {
    // The reverse of an orphan rmeta. A bare rlib with no rmeta is a
    // legitimate state for some crate types (and is harmless even when
    // it isn't). Pruner must never touch non-`.rmeta` files.
    let tmp = tempfile::tempdir().unwrap();
    let deps = tmp.path();
    touch(deps, "libfoo-abc123.rlib");

    assert_eq!(prune_orphan_rmetas_in_deps(deps), 0);
    assert!(exists(deps, "libfoo-abc123.rlib"));
}

#[test]
fn mixed_dir_prunes_only_orphans() {
    let tmp = tempfile::tempdir().unwrap();
    let deps = tmp.path();

    // Paired (keep).
    touch(deps, "libserde-2d37763.rmeta");
    touch(deps, "libserde-2d37763.rlib");
    // Orphan (delete).
    touch(deps, "libsoldr_core-bc09ee.rmeta");
    // Proc-macro pair (keep).
    touch(deps, "serde_derive-abc123.rmeta");
    touch(deps, "serde_derive-abc123.dll");
    // Another orphan (delete).
    touch(deps, "libtoml-deadbeef.rmeta");
    // Bare rlib (keep — untouched, not even considered).
    touch(deps, "libonly_rlib-cafef00d.rlib");
    // Bare .d (untouched).
    touch(deps, "stray-12345.d");

    let deleted = prune_orphan_rmetas_in_deps(deps);
    assert_eq!(deleted, 2, "exactly two orphan rmetas should be deleted");

    assert!(exists(deps, "libserde-2d37763.rmeta"));
    assert!(exists(deps, "libserde-2d37763.rlib"));
    assert!(!exists(deps, "libsoldr_core-bc09ee.rmeta"));
    assert!(exists(deps, "serde_derive-abc123.rmeta"));
    assert!(exists(deps, "serde_derive-abc123.dll"));
    assert!(!exists(deps, "libtoml-deadbeef.rmeta"));
    assert!(exists(deps, "libonly_rlib-cafef00d.rlib"));
    assert!(exists(deps, "stray-12345.d"));
}

#[test]
fn hash_collision_twins_evaluated_independently() {
    // The exact failure shape from soldr#410: cargo invalidates the
    // existing crate fingerprint between two builds and a new hash
    // gets recompiled. The previous (paired) artifact stays; the
    // newer (failed) build leaves an orphan rmeta. Pruning must keep
    // the paired set and remove only the orphan twin.
    let tmp = tempfile::tempdir().unwrap();
    let deps = tmp.path();
    touch(deps, "libsoldr_core-b246b1f7b60ba827.rmeta");
    touch(deps, "libsoldr_core-b246b1f7b60ba827.rlib");
    touch(deps, "libsoldr_core-bc09ee1008e7e294.rmeta"); // orphan twin

    assert_eq!(prune_orphan_rmetas_in_deps(deps), 1);
    assert!(exists(deps, "libsoldr_core-b246b1f7b60ba827.rmeta"));
    assert!(exists(deps, "libsoldr_core-b246b1f7b60ba827.rlib"));
    assert!(!exists(deps, "libsoldr_core-bc09ee1008e7e294.rmeta"));
}

#[test]
fn files_in_subdirectories_are_ignored() {
    // `target/<profile>/deps/` does not normally contain
    // subdirectories, but cargo and zccache occasionally drop nested
    // structures (e.g., `incremental/`). The pruner walks deps/
    // shallow and must not descend into subdirs — a stray orphan
    // rmeta inside an incremental cache or a side-by-side build
    // directory has different semantics and is not our concern.
    let tmp = tempfile::tempdir().unwrap();
    let deps = tmp.path();
    let nested = deps.join("nested");
    fs::create_dir_all(&nested).unwrap();
    touch(&nested, "libfoo-deadbeef.rmeta");
    // Sibling orphan at the top level — must still be pruned.
    touch(deps, "libbar-cafef00d.rmeta");

    assert_eq!(prune_orphan_rmetas_in_deps(deps), 1);
    assert!(exists(&nested, "libfoo-deadbeef.rmeta"));
    assert!(!exists(deps, "libbar-cafef00d.rmeta"));
}

#[test]
fn non_rmeta_files_are_never_touched() {
    // The pruner must not act on any extension other than `.rmeta`.
    // Crate-author convention emits `.d` dep-info files, `.rlib`,
    // `.so`/`.dll`/`.dylib`, and `.exe` outputs, none of which we own.
    let tmp = tempfile::tempdir().unwrap();
    let deps = tmp.path();
    touch(deps, "libfoo-1.d");
    touch(deps, "libfoo-1.rlib");
    touch(deps, "libfoo-1.so");
    touch(deps, "libfoo-1.dll");
    touch(deps, "libfoo-1.dylib");
    touch(deps, "main-2.exe");
    touch(deps, "extension-less-file");

    assert_eq!(prune_orphan_rmetas_in_deps(deps), 0);
    for name in [
        "libfoo-1.d",
        "libfoo-1.rlib",
        "libfoo-1.so",
        "libfoo-1.dll",
        "libfoo-1.dylib",
        "main-2.exe",
        "extension-less-file",
    ] {
        assert!(exists(deps, name), "{name} must not have been touched");
    }
}

#[test]
fn file_path_not_a_directory_is_a_no_op() {
    // Defensive: if the caller hands the pruner a path that happens
    // to be a file (because they passed the wrong arg, or a deps/
    // path was clobbered by a `touch` on Windows AV), do not crash —
    // just return 0 and leave it alone.
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("not-a-dir");
    fs::write(&f, b"hi").unwrap();
    assert_eq!(prune_orphan_rmetas_in_deps(&f), 0);
    assert!(f.exists(), "the bogus file must not be deleted");
}

#[test]
fn find_deps_dirs_covers_host_target_layout() {
    // `target/debug/deps/` (no triple) and `target/release/deps/`
    // (host-target builds) sit two levels under the target root.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("debug").join("deps")).unwrap();
    fs::create_dir_all(root.join("release").join("deps")).unwrap();

    let mut found = find_deps_dirs_for_test(root, 3);
    found.sort();
    assert_eq!(found.len(), 2);
    assert!(found[0].ends_with("debug/deps") || found[0].ends_with("debug\\deps"));
    assert!(found[1].ends_with("release/deps") || found[1].ends_with("release\\deps"));
}

#[test]
fn find_deps_dirs_covers_explicit_target_layout() {
    // `target/<triple>/<profile>/deps/` — the soldr-injected layout
    // sits THREE levels under target/, which is why the default
    // search depth must be at least 3.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(
        root.join("x86_64-pc-windows-msvc")
            .join("debug")
            .join("deps"),
    )
    .unwrap();
    fs::create_dir_all(
        root.join("x86_64-pc-windows-msvc")
            .join("release")
            .join("deps"),
    )
    .unwrap();

    let found = find_deps_dirs_for_test(root, 3);
    assert_eq!(found.len(), 2);
    for p in &found {
        assert!(
            p.ends_with("deps"),
            "every returned path must end with deps: {p:?}"
        );
    }
}

#[test]
fn find_deps_dirs_does_not_descend_into_deps_subtree() {
    // The walker stops descending once it finds a `deps/`. A nested
    // `deps/deps/` (cargo never makes one, but we should be safe
    // against pathological repo state) is reported once, not twice.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("debug").join("deps").join("deps")).unwrap();

    let found = find_deps_dirs_for_test(root, 5);
    assert_eq!(found.len(), 1);
    assert!(found[0].ends_with("debug/deps") || found[0].ends_with("debug\\deps"));
}

#[test]
fn find_deps_dirs_ignores_non_deps_siblings() {
    // `target/<profile>/{build,incremental,examples}/...` must not be
    // misclassified as a deps directory. Only the literal name `deps`
    // matches.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for sibling in ["build", "incremental", "examples", "doc"] {
        fs::create_dir_all(root.join("debug").join(sibling)).unwrap();
    }
    fs::create_dir_all(root.join("debug").join("deps")).unwrap();

    let found = find_deps_dirs_for_test(root, 3);
    assert_eq!(found.len(), 1);
    assert!(found[0].ends_with("debug/deps") || found[0].ends_with("debug\\deps"));
}

#[test]
fn find_deps_dirs_missing_root_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("never-created-target");
    assert!(find_deps_dirs_for_test(&missing, 3).is_empty());
}

#[test]
fn case_insensitive_rmeta_extension_is_recognized() {
    // Windows filesystems are case-insensitive by default and rustc's
    // output is always lowercase, but a user-supplied symlink or a
    // restored backup could conceivably land an `.RMETA` (or `.RLib`)
    // in deps/. Match extensions case-insensitively so the pruner
    // remains predictable.
    let tmp = tempfile::tempdir().unwrap();
    let deps = tmp.path();
    touch(deps, "libfoo-1.RMETA"); // orphan, but uppercase
    touch(deps, "libbar-2.Rmeta");
    touch(deps, "libbar-2.RLIB"); // companion in upper case

    let deleted = prune_orphan_rmetas_in_deps(deps);
    assert_eq!(deleted, 1, "only the bare uppercase rmeta should be pruned");
    assert!(!exists(deps, "libfoo-1.RMETA"));
    assert!(exists(deps, "libbar-2.Rmeta"));
    assert!(exists(deps, "libbar-2.RLIB"));
}
