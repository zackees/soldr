//! Integration tests for `soldr gc target` (issue #574).
//!
//! Exercises the cross-repo workspace walker, JSON output shape, and
//! `--purge --yes` deletion path through the real soldr binary.

#![allow(unused_imports)]

use crate::common;

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn soldr_bin() -> std::path::PathBuf {
    // soldr#1039 phase 1.
    common::soldr_bin()
}

fn seed_workspace(root: &Path, name: &str, target_bytes: usize) -> PathBuf {
    let workspace = root.join(name);
    fs::create_dir_all(&workspace).expect("create workspace dir");
    fs::write(workspace.join("Cargo.toml"), b"[package]\nname=\"x\"\n").expect("Cargo.toml");
    let target = workspace.join("target").join("debug");
    fs::create_dir_all(&target).expect("create target/debug");
    fs::write(target.join("blob.bin"), vec![0u8; target_bytes]).expect("write blob");
    workspace
}

#[test]
fn gc_target_dry_run_json_shape_is_stable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let small = seed_workspace(root, "small", 4 * 1024);
    let large = seed_workspace(root, "large", 1024 * 1024);

    let output = Command::new(soldr_bin())
        .args(["gc", "target", "--root"])
        .arg(root)
        .args(["--dry-run", "--json"])
        .output()
        .expect("failed to run soldr gc target");

    assert!(
        output.status.success(),
        "soldr gc target --dry-run --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        small.join("target").exists(),
        "dry-run must not delete {}",
        small.display()
    );
    assert!(
        large.join("target").exists(),
        "dry-run must not delete {}",
        large.display()
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("gc target --json must be JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "gc target");
    assert_eq!(json["mode"], "report");
    assert_eq!(json["entry_count"], 2);
    assert_eq!(json["purged_count"], 0);
    assert_eq!(json["failed_count"], 0);
    let entries = json["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2);
    // entries are sorted size-desc — large should be first.
    let first_size = entries[0]["size_bytes"].as_u64().unwrap_or(0);
    let second_size = entries[1]["size_bytes"].as_u64().unwrap_or(0);
    assert!(
        first_size >= second_size,
        "entries should be sorted size-desc: {first_size} vs {second_size}"
    );
    assert!(first_size >= 1024 * 1024);

    let total_bytes = json["total_bytes"].as_u64().unwrap_or(0);
    assert!(total_bytes >= 1024 * 1024 + 4 * 1024);
}

#[test]
fn gc_target_purge_yes_deletes_target_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let alpha = seed_workspace(root, "alpha", 8 * 1024);
    let beta = seed_workspace(root, "beta", 16 * 1024);

    let output = Command::new(soldr_bin())
        .args(["gc", "target", "--root"])
        .arg(root)
        .args(["--purge", "--yes", "--json"])
        .output()
        .expect("failed to run soldr gc target --purge --yes");

    assert!(
        output.status.success(),
        "soldr gc target --purge --yes failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !alpha.join("target").exists(),
        "purge must delete {}",
        alpha.display()
    );
    assert!(
        !beta.join("target").exists(),
        "purge must delete {}",
        beta.display()
    );
    // Workspaces themselves stay — only target/ goes away.
    assert!(alpha.join("Cargo.toml").exists());
    assert!(beta.join("Cargo.toml").exists());

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("gc target --purge --json must be JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "gc target");
    assert_eq!(json["mode"], "purge");
    assert_eq!(json["entry_count"], 2);
    assert_eq!(json["purged_count"], 2);
    assert_eq!(json["failed_count"], 0);
}

#[test]
fn gc_target_empty_root_reports_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let output = Command::new(soldr_bin())
        .args(["gc", "target", "--root"])
        .arg(root)
        .args(["--dry-run", "--json"])
        .output()
        .expect("failed to run soldr gc target");

    assert!(
        output.status.success(),
        "soldr gc target on empty root failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(json["entry_count"], 0);
    assert_eq!(json["total_bytes"], 0);
}
