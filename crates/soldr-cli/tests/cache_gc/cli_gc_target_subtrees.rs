//! Integration tests for the in-target subtree GC kinds (#323 slice 4).

#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn sandbox_env(label: &str) -> (PathBuf, PathBuf) {
    (
        unique_temp_dir(&format!("{label}-cargo-home")),
        unique_temp_dir(&format!("{label}-rustup-home")),
    )
}

fn write_file(path: &Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create parent dir");
    }
    fs::write(path, body).expect("failed to write fixture file");
}

fn seed_all_target_subtrees(cache_root: &Path) -> PathBuf {
    let target = seed_gc_candidate(cache_root, "target-subtrees-project");
    write_file(
        &target.join("debug/incremental/s-abc/work-product.bin"),
        b"inc",
    );
    write_file(
        &target.join("debug/build/demo-aaaaaaaaaaaaa/build-script-build.exe"),
        b"build script",
    );
    write_file(&target.join("doc/index.html"), b"docs");
    write_file(&target.join("criterion/report/index.html"), b"criterion");
    target
}

fn seed_one_target_subtree(label: &str, kind: &str) -> (PathBuf, PathBuf) {
    let cache_root = unique_temp_dir(label);
    let target = seed_gc_candidate(&cache_root, label);
    let victim = match kind {
        "cargo_target_incremental" => {
            let path = target.join("debug/incremental");
            write_file(&path.join("s-abc/work-product.bin"), b"inc");
            path
        }
        "cargo_target_build_script_binaries" => {
            let path = target.join("debug/build/demo-aaaaaaaaaaaaa/build-script-build.exe");
            write_file(&path, b"build script");
            path
        }
        "cargo_target_doc" => {
            let path = target.join("doc");
            write_file(&path.join("index.html"), b"docs");
            path
        }
        "cargo_target_subcommand_caches" => {
            let path = target.join("nextest");
            write_file(&path.join("archive.bin"), b"nextest");
            path
        }
        other => panic!("unexpected kind {other}"),
    };
    (cache_root, victim)
}

#[test]
fn gc_list_json_walks_target_subtree_kinds() {
    let cache_root = unique_temp_dir("gc-list-target-subtrees");
    let target = seed_all_target_subtrees(&cache_root);
    let (cargo_home, rustup_home) = sandbox_env("gc-list-target-subtrees");

    let output = common::isolated_soldr_command()
        .args(["gc", "list", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .env("RUSTUP_HOME", &rustup_home)
        .output()
        .expect("failed to run soldr gc list --json");

    assert!(
        output.status.success(),
        "gc list failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("gc list must be JSON");
    let entries = json["entries"].as_array().expect("entries");
    let expected_workspace = target.parent().unwrap().display().to_string();
    for kind in [
        "cargo_target_incremental",
        "cargo_target_build_script_binaries",
        "cargo_target_doc",
        "cargo_target_subcommand_caches",
    ] {
        let entry = entries
            .iter()
            .find(|entry| entry["kind"].as_str() == Some(kind))
            .unwrap_or_else(|| panic!("missing {kind}: {entries:?}"));
        assert_eq!(entry["purge_safety"].as_str(), Some("derived"));
        assert_eq!(
            entry["owner_workspace"].as_str(),
            Some(expected_workspace.as_str())
        );
        assert!(entry["size_bytes"].as_u64().unwrap_or(0) > 0);
        assert!(entry["file_count"].as_u64().unwrap_or(0) > 0);
    }
}

#[test]
fn gc_purge_target_subtree_flags_delete_only_selected_kind() {
    let cases = [
        ("--target-incremental", "cargo_target_incremental"),
        ("--build-scripts", "cargo_target_build_script_binaries"),
        ("--doc", "cargo_target_doc"),
        ("--subcommand-caches", "cargo_target_subcommand_caches"),
    ];

    for (flag, kind) in cases {
        let label = format!("gc-purge-{}", kind.replace('_', "-"));
        let (cache_root, victim) = seed_one_target_subtree(&label, kind);
        assert!(
            victim.exists(),
            "precondition victim exists: {}",
            victim.display()
        );
        let (cargo_home, rustup_home) = sandbox_env(&label);

        let output = common::isolated_soldr_command()
            .args(["gc", "purge", flag, "--all", "--json"])
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("CARGO_HOME", &cargo_home)
            .env("RUSTUP_HOME", &rustup_home)
            .output()
            .expect("failed to run soldr gc purge subtree flag");

        assert!(
            output.status.success(),
            "gc purge {flag} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let json: Value = serde_json::from_slice(&output.stdout).expect("purge must be JSON");
        assert_eq!(json["kind"].as_str(), Some(kind));
        assert_eq!(json["selected_count"].as_u64(), Some(1));
        assert_eq!(json["succeeded_count"].as_u64(), Some(1));
        assert!(
            !victim.exists(),
            "victim should be deleted: {}",
            victim.display()
        );
    }
}
