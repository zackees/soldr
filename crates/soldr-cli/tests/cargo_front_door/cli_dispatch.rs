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
fn version_command_prints_workspace_version() {
    let output = Command::new(common::soldr_bin())
        .arg("version")
        .output()
        .expect("failed to run soldr version");

    assert!(output.status.success(), "version command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        format!("soldr {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn version_command_emits_versioned_json() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("selected-root");
    let output = Command::new(common::soldr_bin())
        .args(["version", "--json"])
        .env("SOLDR_CACHE_DIR", &root)
        .output()
        .expect("failed to run soldr version --json");

    assert!(output.status.success(), "version --json command failed");

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("version --json did not return JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "version");
    assert_eq!(json["soldr_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(PathBuf::from(json["root_dir"].as_str().unwrap()), root);
}

#[test]
fn help_lists_phase_one_command_surface() {
    let output = Command::new(common::soldr_bin())
        .arg("--help")
        .output()
        .expect("failed to run soldr --help");

    assert!(output.status.success(), "help command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("status"), "help output missing status");
    assert!(stdout.contains("clean"), "help output missing clean");
    assert!(stdout.contains("purge"), "help output missing purge");
    assert!(stdout.contains("config"), "help output missing config");
    assert!(stdout.contains("cache"), "help output missing cache");
    assert!(stdout.contains("version"), "help output missing version");
    assert!(stdout.contains("cargo"), "help output missing cargo");
    assert!(stdout.contains("rustup"), "help output missing rustup");
    assert!(
        stdout.contains("toolchain"),
        "help output missing toolchain"
    );
}
