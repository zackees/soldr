//! CACHEDIR.TAG seeding tests for [`super`].
//!
//! In a sibling file rather than an inline `mod`: `dylint_cook.rs` reached the
//! 1000-line production ceiling that `.github/scripts/loc_ceiling.py` enforces,
//! and CLAUDE.md's rule for that is that the addition belongs in a new module.
//! Moving the tests keeps the production surface at its real size -- 855 lines
//! -- rather than counting test scaffolding against it.

use super::*;

/// soldr#2820: without this, `cargo clean --target-dir <dir>` refuses with
/// "missing or invalid `CACHEDIR.TAG` file" and the whole cook fails at its
/// dummy-artifact cleanup step.
#[test]
fn a_freshly_created_target_dir_gets_the_tag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("target").join("dylint");
    std::fs::create_dir_all(&target).expect("create target");

    ensure_cachedir_tag(&target);

    let tag = target.join("CACHEDIR.TAG");
    assert!(tag.is_file(), "cargo will refuse to clean an untagged dir");
    let contents = std::fs::read_to_string(&tag).expect("read tag");
    assert!(
        contents.starts_with("Signature: 8a477f597d28d172789f06886806bc55"),
        "the signature line is fixed by the cachedir spec; got {contents:?}"
    );
}

/// The tag cargo wrote must survive: rewriting it would be soldr asserting
/// ownership of a directory it did not create.
#[test]
fn an_existing_tag_is_left_alone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().to_path_buf();
    let tag = target.join("CACHEDIR.TAG");
    std::fs::write(
        &tag,
        "Signature: 8a477f597d28d172789f06886806bc55\n# cargo's own\n",
    )
    .expect("seed tag");

    ensure_cachedir_tag(&target);

    assert!(
        std::fs::read_to_string(&tag)
            .expect("read tag")
            .contains("cargo's own"),
        "an existing tag must not be overwritten"
    );
}

/// This runs on the way to a build; it must never be the thing that fails
/// one. A path that cannot hold the file is a no-op, not an error.
#[test]
fn an_unwritable_target_dir_is_not_an_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // A path whose parent does not exist: the write fails, and the helper
    // returns anyway.
    ensure_cachedir_tag(&tmp.path().join("absent").join("deeper"));
}
