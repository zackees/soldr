//! What actually blocks a recursive delete on Windows (soldr#2199 / soldr#2200).
//!
//! `gc target --purge` and every other reclaim path in soldr depend on these
//! semantics, and two issues in a row were argued from a wrong model of them:
//!
//! - soldr#2199 attributed a failed purge to zccache's read-only hardlinked
//!   CAS artifacts, on the strength of a PowerShell repro. `Remove-Item` and
//!   `[IO.Directory]::Delete` really do refuse a read-only file — but Rust's
//!   `remove_dir_all` deletes with POSIX semantics and an explicit
//!   ignore-readonly disposition, so the attribute is bypassed.
//! - soldr#2200 then proposed banning `std::fs::remove_dir_all` and
//!   `std::fs::remove_file` workspace-wide across 72 call sites, routing them
//!   all through a helper whose read-only retry provably never fires.
//!
//! Prose disproofs get re-litigated; a test does not. This states the
//! contested behaviour in code so that if it is ever *actually* false — a
//! toolchain change, a different filesystem, a future Windows — the answer is
//! a red test naming the assumption rather than another investigation from
//! first principles.
//!
//! Scope: only what was contested. The conditions that *do* block a delete
//! (a process whose cwd is inside the tree, a running executable mapped from
//! it) are recorded on soldr#2199; they need a live child process to observe
//! and are a poor fit for a unit test.

use std::fs;
use std::path::Path;

fn make_read_only(path: &Path) {
    let mut perms = fs::metadata(path).expect("stat").permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms).expect("set read-only");
    assert!(
        fs::metadata(path).expect("stat").permissions().readonly(),
        "fixture must actually be read-only: {}",
        path.display(),
    );
}

// The shape soldr#2199 reported: a cache-restored artifact under `deps/`
// carrying the read-only attribute it shares with the CAS object.
#[test]
fn read_only_files_do_not_block_a_recursive_delete() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target");
    let deps = target
        .join("x86_64-pc-windows-msvc")
        .join("debug")
        .join("deps");
    fs::create_dir_all(&deps).expect("mkdir");

    let artifact = deps.join("libsoldr_cache-40bc200be77f3984.rlib");
    fs::write(&artifact, b"cas artifact").expect("write");
    make_read_only(&artifact);

    fs::remove_dir_all(&target).expect(
        "std::fs::remove_dir_all must delete through the read-only attribute; \
         if this fails, the soldr#2199 read-only theory is live after all and \
         the disproof on that issue needs revisiting",
    );
    assert!(!target.exists());
}

// soldr#2200 extends the proposed ban to `remove_file` on the same grounds,
// so the same claim is checked for it.
#[test]
fn read_only_files_do_not_block_a_single_file_delete() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let file = temp.path().join("libfoo.rlib");
    fs::write(&file, b"x").expect("write");
    make_read_only(&file);

    fs::remove_file(&file).expect(
        "std::fs::remove_file must delete through the read-only attribute; \
         if this fails, the soldr#2200 premise holds for remove_file",
    );
    assert!(!file.exists());
}

// A read-only *directory* is the case neither issue tested, and it is the one
// that would still surprise: the attribute means something different on a
// directory, so the answer is not implied by the file cases above.
#[test]
fn a_read_only_directory_does_not_block_its_parents_delete() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target");
    let nested = target.join("debug").join("incremental");
    fs::create_dir_all(&nested).expect("mkdir");
    fs::write(nested.join("blob.bin"), b"x").expect("write");
    make_read_only(&nested);

    fs::remove_dir_all(&target).expect("a read-only directory must not block the delete either");
    assert!(!target.exists());
}
