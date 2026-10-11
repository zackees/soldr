//! soldr#3712: the atomic temp-then-rename archive write (soldr#3687) must
//! not leave the archive at the temp file's private 0o600 mode.
//!
//! Hermetic without touching the process umask: the expected fresh-archive
//! mode is whatever a plain `File::create` produces under the current umask
//! (0o666 & !umask), measured on a sibling file. On hosts without mode
//! semantics `soldr_platform` reports `None` and there is nothing to compare.

use super::*;
use soldr_platform::fs::permissions::{mode, restore_mode};

fn write_dummy(out: &Path) {
    write_archive_file(out, |mut f| {
        use std::io::Write as _;
        f.write_all(b"archive").map_err(SaveLoadError::BareIo)
    })
    .unwrap();
}

#[test]
fn fresh_archive_gets_plain_create_mode_not_tempfile_private_mode() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline");
    File::create(&baseline).unwrap();
    let out = dir.path().join("fresh.tar.zst");
    write_dummy(&out);
    let (Some(expected), Some(actual)) = (mode(&baseline), mode(&out)) else {
        return;
    };
    assert_eq!(
        actual & 0o7777,
        expected & 0o7777,
        "archive mode {:o} should match File::create mode {:o}",
        actual & 0o7777,
        expected & 0o7777
    );
}

#[test]
fn existing_archive_mode_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("existing.tar.zst");
    std::fs::write(&out, b"old").unwrap();
    restore_mode(&out, Some(0o640)).unwrap();
    write_dummy(&out);
    if let Some(actual) = mode(&out) {
        assert_eq!(actual & 0o7777, 0o640);
    }
}
