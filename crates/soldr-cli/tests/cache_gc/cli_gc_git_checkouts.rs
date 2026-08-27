//! Integration tests for the cargo_git_checkouts walker (#323 slice 3).
//!
//! Sandbox `$CARGO_HOME` to a tempdir so the walker never touches the
//! developer's real `~/.cargo`.

#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Construct a fake `$CARGO_HOME` containing `git/checkouts/<repo>/` so
/// the walker has somewhere to look.
fn fresh_cargo_home(label: &str) -> PathBuf {
    let cargo_home = unique_temp_dir(label);
    fs::create_dir_all(cargo_home.join("git").join("checkouts"))
        .expect("failed to create git/checkouts root");
    cargo_home
}

/// Seed a per-commit checkout directory under
/// `$CARGO_HOME/git/checkouts/<repo>/<commit>/` with one file inside so
/// size is non-zero.
fn seed_git_checkout(cargo_home: &Path, repo_dir: &str, commit_dir: &str) -> PathBuf {
    let dir = cargo_home
        .join("git")
        .join("checkouts")
        .join(repo_dir)
        .join(commit_dir);
    fs::create_dir_all(&dir).expect("failed to create checkout dir");
    fs::write(dir.join("README.md"), b"// vendored git checkout\n")
        .expect("failed to seed checkout file");
    dir
}

#[test]
fn gc_list_json_walks_cargo_git_checkouts_under_cargo_home() {
    let cache_root = unique_temp_dir("gc-git-checkouts-walk-cache");
    let cargo_home = fresh_cargo_home("gc-git-checkouts-walk-cargo");
    let _a = seed_git_checkout(&cargo_home, "tokio-abc123", "deadbeef1234");
    let _b = seed_git_checkout(&cargo_home, "serde-def456", "cafefacedeed");

    let output = Command::new(common::soldr_bin())
        .args(["gc", "list", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .output()
        .expect("failed to run soldr gc list --json");

    assert!(
        output.status.success(),
        "gc list --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("gc list --json must produce JSON");
    let entries = json["entries"].as_array().expect("entries array");
    let checkouts: Vec<&Value> = entries
        .iter()
        .filter(|e| e["kind"].as_str() == Some("cargo_git_checkouts"))
        .collect();
    assert_eq!(
        checkouts.len(),
        2,
        "expected two cargo_git_checkouts entries, got: {}",
        serde_json::to_string_pretty(&entries).unwrap_or_default()
    );

    for entry in &checkouts {
        assert_eq!(
            entry["purge_safety"].as_str(),
            Some("derived"),
            "git checkouts must be derived class"
        );
        assert!(
            entry["owner_crate"]
                .as_str()
                .map(|s| s.contains('@'))
                .unwrap_or(false),
            "owner_crate must contain repo@commit: {entry:?}"
        );
        assert_eq!(entry["last_used_source"].as_str(), Some("fs_mtime"));
        assert!(entry["size_bytes"].as_u64().unwrap_or(0) > 0);
    }
}

#[test]
fn gc_list_json_kind_filter_narrows_to_cargo_git_checkouts() {
    let cache_root = unique_temp_dir("gc-git-checkouts-filter-cache");
    let cargo_home = fresh_cargo_home("gc-git-checkouts-filter-cargo");
    let _x = seed_git_checkout(&cargo_home, "tokio-abc", "abc123");

    let output = Command::new(common::soldr_bin())
        .args(["gc", "list", "--json", "--kind", "cargo_git_checkouts"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .output()
        .expect("failed to run soldr gc list --json --kind cargo_git_checkouts");

    assert!(output.status.success());
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("gc list --json must produce JSON");
    let entries = json["entries"].as_array().expect("entries array");
    assert!(
        entries
            .iter()
            .all(|e| e["kind"].as_str() == Some("cargo_git_checkouts")),
        "kind filter must return only cargo_git_checkouts: {entries:?}"
    );
    assert_eq!(entries.len(), 1);
}

#[test]
fn gc_purge_git_checkouts_removes_checkout_directories() {
    let cache_root = unique_temp_dir("gc-git-checkouts-purge-cache");
    let cargo_home = fresh_cargo_home("gc-git-checkouts-purge-cargo");
    let dir = seed_git_checkout(&cargo_home, "tokio-abc", "deadbeef");
    assert!(dir.is_dir());

    let output = Command::new(common::soldr_bin())
        .args(["gc", "purge", "--git-checkouts", "--all", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .output()
        .expect("failed to run soldr gc purge --git-checkouts --all --json");

    assert!(
        output.status.success(),
        "gc purge --git-checkouts failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("gc purge --json must produce JSON");
    assert_eq!(json["mode"], "purge");
    assert_eq!(json["kind"], "cargo_git_checkouts");
    assert_eq!(json["selected_count"], 1);
    assert_eq!(json["succeeded_count"], 1);
    assert!(
        json["reclaimed_bytes"].as_u64().unwrap_or(0) > 0,
        "reclaimed_bytes must be non-zero: {json}"
    );
    assert!(!dir.exists(), "checkout dir must be removed after purge");
}

#[test]
fn gc_purge_rejects_both_registry_src_and_git_checkouts() {
    let cache_root = unique_temp_dir("gc-purge-mutex-cache");
    let output = Command::new(common::soldr_bin())
        .args([
            "gc",
            "purge",
            "--registry-src",
            "--git-checkouts",
            "--all",
            "--json",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc purge --registry-src --git-checkouts");

    assert_ne!(
        output.status.code(),
        Some(0),
        "purge should reject both flags being set"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be used")
            || stderr.contains("conflicts with")
            || stderr.contains("conflict"),
        "expected clap mutual-exclusion error in stderr:\n{stderr}"
    );
}
