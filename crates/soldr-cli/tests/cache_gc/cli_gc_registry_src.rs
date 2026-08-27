//! Integration tests for the cargo_registry_src walker (#323 slice 2).
//!
//! These tests sandbox `$CARGO_HOME` to a tempdir so the walker never
//! touches the developer's real `~/.cargo`.

#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Construct a fake `$CARGO_HOME` containing a `registry/src/<reg>`
/// scaffold so the walker has somewhere to look. Returns the
/// `$CARGO_HOME` root.
fn fresh_cargo_home(label: &str) -> PathBuf {
    let cargo_home = unique_temp_dir(label);
    let reg_root = cargo_home
        .join("registry")
        .join("src")
        .join("index.crates.io-abc123");
    fs::create_dir_all(&reg_root).expect("failed to create registry src root");
    cargo_home
}

/// Seed a single `<crate>-<vers>` directory under a previously-created
/// `registry/src/<reg>/` root, with one file inside so size is non-zero.
fn seed_registry_src_crate(cargo_home: &Path, crate_dir_name: &str) -> PathBuf {
    let reg_root = cargo_home
        .join("registry")
        .join("src")
        .join("index.crates.io-abc123");
    fs::create_dir_all(&reg_root).expect("failed to create registry src root");
    let dir = reg_root.join(crate_dir_name);
    fs::create_dir_all(&dir).expect("failed to create crate dir");
    fs::write(dir.join("lib.rs"), b"// vendored crate source\n")
        .expect("failed to seed crate file");
    dir
}

#[test]
fn gc_list_json_walks_cargo_registry_src_under_cargo_home() {
    let cache_root = unique_temp_dir("gc-reg-src-walk-cache");
    let cargo_home = fresh_cargo_home("gc-reg-src-walk-cargo");
    let serde_dir = seed_registry_src_crate(&cargo_home, "serde-1.0.213");
    let chrono_tz_dir = seed_registry_src_crate(&cargo_home, "chrono-tz-0.8.1");

    let output = common::isolated_soldr_command()
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

    let json: Value = serde_json::from_slice(&output.stdout).expect("gc list --json must be JSON");
    let entries = json["entries"].as_array().expect("entries array");

    let reg_src_entries: Vec<&Value> = entries
        .iter()
        .filter(|e| e["kind"].as_str() == Some("cargo_registry_src"))
        .collect();
    assert_eq!(
        reg_src_entries.len(),
        2,
        "expected exactly two cargo_registry_src entries, got: {}",
        serde_json::to_string_pretty(entries).unwrap()
    );

    let serde_canonical = fs::canonicalize(&serde_dir).unwrap_or(serde_dir.clone());
    let chrono_tz_canonical = fs::canonicalize(&chrono_tz_dir).unwrap_or(chrono_tz_dir.clone());

    let serde_entry = reg_src_entries
        .iter()
        .find(|e| {
            let p = e["path"].as_str().unwrap_or("");
            let pb = PathBuf::from(p);
            pb == serde_dir
                || pb == serde_canonical
                || fs::canonicalize(&pb).ok().as_deref() == Some(serde_canonical.as_path())
        })
        .expect("serde entry not present in registry_src walk");
    let chrono_tz_entry = reg_src_entries
        .iter()
        .find(|e| {
            let p = e["path"].as_str().unwrap_or("");
            let pb = PathBuf::from(p);
            pb == chrono_tz_dir
                || pb == chrono_tz_canonical
                || fs::canonicalize(&pb).ok().as_deref() == Some(chrono_tz_canonical.as_path())
        })
        .expect("chrono-tz entry not present in registry_src walk");

    assert_eq!(
        serde_entry["owner_crate"].as_str(),
        Some("serde@1.0.213"),
        "serde owner_crate parsed wrong"
    );
    assert_eq!(
        chrono_tz_entry["owner_crate"].as_str(),
        Some("chrono-tz@0.8.1"),
        "chrono-tz owner_crate must use last-hyphen-before-digit rule"
    );

    for entry in &reg_src_entries {
        assert_eq!(
            entry["purge_safety"].as_str(),
            Some("derived"),
            "registry_src entries must be derived"
        );
        assert!(
            entry["size_bytes"].as_u64().is_some(),
            "size_bytes must be present"
        );
        assert!(
            entry["file_count"].as_u64().is_some(),
            "file_count must be present"
        );
    }
}

#[test]
fn gc_list_json_kind_filter_narrows_to_cargo_target() {
    let cache_root = unique_temp_dir("gc-list-kind-target");
    let target = seed_gc_candidate(&cache_root, "kind-target-project");
    let cargo_home = fresh_cargo_home("gc-list-kind-target-cargo");
    let serde_dir = seed_registry_src_crate(&cargo_home, "serde-1.0.213");

    let output = common::isolated_soldr_command()
        .args(["gc", "list", "--json", "--kind", "cargo_target"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .output()
        .expect("failed to run soldr gc list --json --kind cargo_target");

    assert!(
        output.status.success(),
        "gc list --kind cargo_target failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("gc list must be JSON");
    let entries = json["entries"].as_array().expect("entries array");
    assert!(!entries.is_empty(), "expected at least one cargo_target");
    for entry in entries {
        assert_eq!(
            entry["kind"].as_str(),
            Some("cargo_target"),
            "filter must drop non-cargo_target entries: {entry}"
        );
    }
    let _ = target;
    let _ = serde_dir;
}

#[test]
fn gc_list_json_kind_filter_narrows_to_cargo_registry_src() {
    let cache_root = unique_temp_dir("gc-list-kind-regsrc");
    let _target = seed_gc_candidate(&cache_root, "kind-regsrc-project");
    let cargo_home = fresh_cargo_home("gc-list-kind-regsrc-cargo");
    let _serde_dir = seed_registry_src_crate(&cargo_home, "serde-1.0.213");

    let output = common::isolated_soldr_command()
        .args(["gc", "list", "--json", "--kind", "cargo_registry_src"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .output()
        .expect("failed to run soldr gc list --json --kind cargo_registry_src");

    assert!(
        output.status.success(),
        "gc list --kind cargo_registry_src failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("gc list must be JSON");
    let entries = json["entries"].as_array().expect("entries array");
    assert!(
        !entries.is_empty(),
        "expected at least one cargo_registry_src entry"
    );
    for entry in entries {
        assert_eq!(
            entry["kind"].as_str(),
            Some("cargo_registry_src"),
            "filter must drop non-registry_src entries: {entry}"
        );
    }
}

#[test]
fn gc_list_unknown_kind_value_is_rejected() {
    let cache_root = unique_temp_dir("gc-list-bogus-kind");
    let output = common::isolated_soldr_command()
        .args(["gc", "list", "--json", "--kind", "bogus"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc list --kind bogus");

    assert!(
        !output.status.success(),
        "unknown --kind value must be rejected with non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bogus"),
        "stderr should mention the bad --kind value, got: {stderr}"
    );
}

#[test]
fn gc_purge_registry_src_all_deletes_walked_dirs() {
    let cache_root = unique_temp_dir("gc-purge-regsrc-cache");
    let cargo_home = fresh_cargo_home("gc-purge-regsrc-cargo");
    let serde_dir = seed_registry_src_crate(&cargo_home, "serde-1.0.213");
    assert!(serde_dir.exists(), "precondition: seeded dir must exist");

    let output = common::isolated_soldr_command()
        .args(["gc", "purge", "--registry-src", "--all", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .output()
        .expect("failed to run soldr gc purge --registry-src --all");

    assert!(
        output.status.success(),
        "gc purge --registry-src --all failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !serde_dir.exists(),
        "registry_src dir {} must be removed",
        serde_dir.display()
    );
}

#[test]
fn gc_purge_without_kind_does_not_touch_registry_src() {
    let cache_root = unique_temp_dir("gc-purge-default-cache");
    let target = seed_gc_candidate(&cache_root, "default-purge-project");
    let cargo_home = fresh_cargo_home("gc-purge-default-cargo");
    let serde_dir = seed_registry_src_crate(&cargo_home, "serde-1.0.213");
    assert!(serde_dir.exists());
    assert!(target.exists());

    let output = common::isolated_soldr_command()
        .args([
            "gc",
            "purge",
            "--all",
            "--older-than",
            "1s",
            "--larger-than",
            "1B",
            "--json",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .output()
        .expect("failed to run soldr gc purge --all (default)");

    assert!(
        output.status.success(),
        "gc purge --all failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Back-compat: registry_src must be untouched without --kind/--registry-src.
    assert!(
        serde_dir.exists(),
        "registry_src dir {} must survive default purge (back-compat)",
        serde_dir.display()
    );

    // The cargo_target row should have been processed; the seeded dir is deleted.
    assert!(
        !target.exists(),
        "cargo_target {} should have been deleted by default purge",
        target.display()
    );
}
