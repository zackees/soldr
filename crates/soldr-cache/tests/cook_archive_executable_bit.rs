//! #1880 regression: cook-archive extraction must restore the
//! executable bit on `build-script-build` binaries. Unix-only — the
//! mode bits the archive records only carry meaning there.

use std::io::Write;
use std::path::Path;

use soldr_cache::cache_lib::cook_archive::{extract_skip_existing, pack_cook_archive};
use tempfile::TempDir;

fn write_file(p: &Path, bytes: &[u8]) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    let mut f = std::fs::File::create(p).expect("create");
    f.write_all(bytes).expect("write");
}

// #1880: cargo's `build-script-build` binaries must come back
// executable. The extractor hand-copies bytes into a fresh
// `File::create` (to honor skip-existing), which lands at the
// umask default and silently drops `+x` unless the tar header's
// mode is applied explicitly.
//
// The consequence in the field is badly disguised: cargo reports
// `could not execute process ... (never executed)` /
// `Permission denied (os error 13)` naming whichever build script
// it happened to reach first, so the failure looks like a flaky
// problem with an unrelated third-party crate.
#[test]
fn extract_preserves_executable_bit() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let dir = TempDir::new().expect("tempdir");
    let source = dir.path().join("debug");
    let script = source
        .join("build")
        .join("foo-abc")
        .join("build-script-build");
    write_file(&script, b"#!/bin/sh\nexit 0\n");
    soldr_platform::fs::permissions::make_executable(&script).expect("chmod source");
    // A non-executable sibling proves we restore the recorded mode
    // rather than blanket-chmod'ing everything.
    write_file(&source.join("deps").join("libfoo-abc.rlib"), b"foo\n");

    let cook = dir.path().join("cook");
    let packed = pack_cook_archive(&source, &cook).expect("pack");

    let dest = dir.path().join("target");
    std::fs::create_dir_all(&dest).unwrap();
    extract_skip_existing(&packed.path, &dest).expect("extract");

    let restored = dest
        .join("debug")
        .join("build")
        .join("foo-abc")
        .join("build-script-build");
    let mode = soldr_platform::fs::permissions::mode(&restored).expect("stat mode");
    assert_ne!(
        mode & 0o111,
        0,
        "build-script-build restored without +x -- cargo would fail \
         execve with EACCES (regression of #1880)"
    );

    let rlib = dest.join("debug").join("deps").join("libfoo-abc.rlib");
    let rlib_mode = soldr_platform::fs::permissions::mode(&rlib).expect("stat mode");
    assert_eq!(
        rlib_mode & 0o111,
        0,
        "a non-executable input must stay non-executable"
    );
}
