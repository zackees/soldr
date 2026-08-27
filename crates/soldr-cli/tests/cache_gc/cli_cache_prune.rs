#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::io::Write;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[test]
fn cache_prune_target_dry_run_emits_json() {
    let cache_root = unique_temp_dir("cache-prune-target-json");
    let target = cache_root.join("target");
    let parent = target.join("debug").join("deps");
    fs::create_dir_all(&parent).expect("failed to create deps dir");
    // Use file entries (with the cargo-style `.rlib` extension) rather
    // than directories — `set_modified` on a directory handle requires
    // privileged access on Windows.
    let older = parent.join("libfoo-aaaaaaaaaaaaa.rlib");
    let newer = parent.join("libfoo-bbbbbbbbbbbbb.rlib");
    fs::write(&older, b"older").expect("failed to seed older content");
    fs::write(&newer, b"newer-with-more-bytes").expect("failed to seed newer content");

    // Pin mtimes so the test is deterministic on slow filesystems.
    let older_when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let newer_when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_500);
    {
        let f = fs::File::options()
            .write(true)
            .open(&older)
            .expect("open older for mtime");
        f.set_modified(older_when).expect("set older mtime");
    }
    {
        let f = fs::File::options()
            .write(true)
            .open(&newer)
            .expect("open newer for mtime");
        f.set_modified(newer_when).expect("set newer mtime");
    }

    let output = Command::new(common::soldr_bin())
        .args(["cache", "prune-target"])
        .arg(&target)
        .args(["--dry-run", "--json"])
        .output()
        .expect("failed to run soldr cache prune-target --dry-run --json");

    assert!(
        output.status.success(),
        "cache prune-target failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Dry-run must leave everything on disk.
    assert!(older.exists(), "dry-run must not delete older entry");
    assert!(newer.exists(), "dry-run must not delete newer entry");

    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("cache prune-target --json must produce parseable JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "cache prune-target");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["scanned"], 2);
    assert_eq!(json["kept"], 1);
    assert_eq!(json["deleted"], 1);

    let entries = json["entries"]
        .as_array()
        .expect("entries must be an array");
    assert_eq!(entries.len(), 2);
    let mut found_keep = false;
    let mut found_delete = false;
    for entry in entries {
        for field in [
            "path",
            "prefix",
            "hash",
            "size_bytes",
            "size_human",
            "mtime_unix",
            "action",
        ] {
            assert!(
                entry.get(field).is_some(),
                "prune-target entry missing field {field}: {entry}"
            );
        }
        assert_eq!(entry["prefix"], "libfoo");
        match entry["action"].as_str() {
            Some("keep") => {
                found_keep = true;
                assert_eq!(entry["hash"], "bbbbbbbbbbbbb");
            }
            Some("delete") => {
                found_delete = true;
                assert_eq!(entry["hash"], "aaaaaaaaaaaaa");
            }
            other => panic!("unexpected action {other:?} in {entry}"),
        }
    }
    assert!(found_keep, "must have one keep entry");
    assert!(found_delete, "must have one delete entry");
}
