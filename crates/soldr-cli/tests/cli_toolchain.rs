#![allow(unused_imports)]

mod common;

use common::*;
use serde_json::Value;
use std::io::Write;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[test]
fn rustup_passthrough_forwards_args_unchanged_for_unscoped_subcommands() {
    let workspace = unique_temp_dir("rustup-passthrough-show");
    let log_path = workspace.join("rustup.log");
    let rustup = install_logging_fake_rustup(&log_path);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["rustup", "show"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .output()
        .expect("failed to run soldr rustup show");

    assert!(
        output.status.success(),
        "soldr rustup show failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let invocations = read_logged_rustup_invocations(&log_path);
    assert_eq!(invocations.len(), 1, "expected one rustup invocation");
    assert_eq!(invocations[0], vec!["show".to_string()]);
}

#[test]
fn rustup_passthrough_injects_toolchain_for_target_add() {
    let workspace = unique_temp_dir("rustup-passthrough-target-add");
    seed_rust_toolchain_toml(&workspace, "[toolchain]\nchannel = \"1.94.1\"\n");
    let log_path = workspace.join("rustup.log");
    let rustup = install_logging_fake_rustup(&log_path);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["rustup", "target", "add", "x86_64-unknown-linux-musl"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .output()
        .expect("failed to run soldr rustup target add");

    assert!(
        output.status.success(),
        "soldr rustup target add failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let invocations = read_logged_rustup_invocations(&log_path);
    assert_eq!(invocations.len(), 1, "expected one rustup invocation");
    assert_eq!(
        invocations[0],
        vec![
            "target".to_string(),
            "add".to_string(),
            "--toolchain".to_string(),
            "1.94.1".to_string(),
            "x86_64-unknown-linux-musl".to_string(),
        ]
    );
}

#[test]
fn rustup_passthrough_does_not_double_inject_toolchain() {
    let workspace = unique_temp_dir("rustup-passthrough-explicit-toolchain");
    seed_rust_toolchain_toml(&workspace, "[toolchain]\nchannel = \"1.94.1\"\n");
    let log_path = workspace.join("rustup.log");
    let rustup = install_logging_fake_rustup(&log_path);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args([
            "rustup",
            "target",
            "add",
            "--toolchain",
            "nightly",
            "aarch64-apple-darwin",
        ])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .output()
        .expect("failed to run soldr rustup target add --toolchain nightly");

    assert!(
        output.status.success(),
        "soldr rustup target add --toolchain failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let invocations = read_logged_rustup_invocations(&log_path);
    assert_eq!(invocations.len(), 1, "expected one rustup invocation");
    let invocation = &invocations[0];
    let toolchain_count = invocation
        .iter()
        .filter(|arg| *arg == "--toolchain")
        .count();
    assert_eq!(
        toolchain_count, 1,
        "--toolchain should appear exactly once: {invocation:?}"
    );
    let toolchain_value_idx = invocation
        .iter()
        .position(|arg| arg == "--toolchain")
        .expect("--toolchain not found");
    assert_eq!(
        invocation.get(toolchain_value_idx + 1).map(String::as_str),
        Some("nightly"),
        "user-supplied toolchain should be preserved: {invocation:?}"
    );
}

#[test]
fn toolchain_install_invokes_rustup_with_channel() {
    let workspace = unique_temp_dir("toolchain-install");
    seed_rust_toolchain_toml(&workspace, "[toolchain]\nchannel = \"1.94.1\"\n");
    let log_path = workspace.join("rustup.log");
    let rustup = install_logging_fake_rustup(&log_path);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["toolchain", "install"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .output()
        .expect("failed to run soldr toolchain install");

    assert!(
        output.status.success(),
        "soldr toolchain install failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let invocations = read_logged_rustup_invocations(&log_path);
    assert_eq!(invocations.len(), 1, "expected one rustup invocation");
    assert_eq!(
        invocations[0],
        vec![
            "toolchain".to_string(),
            "install".to_string(),
            "1.94.1".to_string(),
            "--profile".to_string(),
            "minimal".to_string(),
            "--no-self-update".to_string(),
        ]
    );
}

#[test]
fn toolchain_prepare_installs_channel_components_and_targets() {
    let workspace = unique_temp_dir("toolchain-prepare");
    seed_rust_toolchain_toml(
        &workspace,
        "[toolchain]\n\
         channel = \"1.94.1\"\n\
         components = [\"clippy\"]\n\
         targets = [\"x86_64-unknown-linux-musl\"]\n",
    );
    let log_path = workspace.join("rustup.log");
    let rustup = install_logging_fake_rustup(&log_path);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["toolchain", "prepare"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .output()
        .expect("failed to run soldr toolchain prepare");

    assert!(
        output.status.success(),
        "soldr toolchain prepare failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let invocations = read_logged_rustup_invocations(&log_path);
    assert_eq!(
        invocations.len(),
        3,
        "expected install + component add + target add: {invocations:?}"
    );
    assert_eq!(
        invocations[0],
        vec![
            "toolchain".to_string(),
            "install".to_string(),
            "1.94.1".to_string(),
            "--profile".to_string(),
            "minimal".to_string(),
            "--no-self-update".to_string(),
        ],
        "first invocation should install the pinned channel"
    );
    assert_eq!(
        invocations[1],
        vec![
            "component".to_string(),
            "add".to_string(),
            "--toolchain".to_string(),
            "1.94.1".to_string(),
            "clippy".to_string(),
        ],
        "second invocation should add the declared component"
    );
    assert_eq!(
        invocations[2],
        vec![
            "target".to_string(),
            "add".to_string(),
            "--toolchain".to_string(),
            "1.94.1".to_string(),
            "x86_64-unknown-linux-musl".to_string(),
        ],
        "third invocation should add the declared target"
    );
}

#[test]
fn toolchain_prepare_installs_plugins_with_version() {
    let workspace = unique_temp_dir("toolchain-prepare-plugin-version");
    seed_rust_toolchain_toml(
        &workspace,
        "[toolchain]\n\
         channel = \"1.94.1\"\n\
         \n\
         [soldr.plugins]\n\
         cargo-nextest = \"0.9\"\n",
    );
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let rustup = install_logging_fake_rustup(&rustup_log);
    let cargo = install_logging_fake_cargo(&cargo_log);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["toolchain", "prepare"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .output()
        .expect("failed to run soldr toolchain prepare");

    assert!(
        output.status.success(),
        "soldr toolchain prepare failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let cargo_invocations = read_logged_cargo_invocations(&cargo_log);
    assert_eq!(
        cargo_invocations.len(),
        1,
        "expected exactly one cargo install: {cargo_invocations:?}"
    );
    let invocation = &cargo_invocations[0];
    assert_eq!(invocation.first().map(String::as_str), Some("install"));
    assert_eq!(invocation.get(1).map(String::as_str), Some("cargo-nextest"));
    let version_idx = invocation
        .iter()
        .position(|arg| arg == "--version")
        .expect("--version should appear");
    assert_eq!(
        invocation.get(version_idx + 1).map(String::as_str),
        Some("0.9")
    );
}

#[test]
fn toolchain_prepare_installs_plugin_with_locked_flag() {
    let workspace = unique_temp_dir("toolchain-prepare-plugin-locked");
    seed_rust_toolchain_toml(
        &workspace,
        "[toolchain]\n\
         channel = \"1.94.1\"\n\
         \n\
         [soldr.plugins]\n\
         cargo-zigbuild = { version = \"0.18\", locked = true }\n",
    );
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let rustup = install_logging_fake_rustup(&rustup_log);
    let cargo = install_logging_fake_cargo(&cargo_log);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["toolchain", "prepare"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .output()
        .expect("failed to run soldr toolchain prepare");

    assert!(
        output.status.success(),
        "soldr toolchain prepare failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let cargo_invocations = read_logged_cargo_invocations(&cargo_log);
    assert_eq!(
        cargo_invocations.len(),
        1,
        "expected exactly one cargo install: {cargo_invocations:?}"
    );
    let invocation = &cargo_invocations[0];
    assert_eq!(invocation.first().map(String::as_str), Some("install"));
    assert_eq!(
        invocation.get(1).map(String::as_str),
        Some("cargo-zigbuild")
    );
    let version_idx = invocation
        .iter()
        .position(|arg| arg == "--version")
        .expect("--version should appear");
    assert_eq!(
        invocation.get(version_idx + 1).map(String::as_str),
        Some("0.18")
    );
    assert!(
        invocation.iter().any(|arg| arg == "--locked"),
        "expected --locked in argv: {invocation:?}"
    );
}

#[test]
fn toolchain_prepare_plugin_without_version_uses_no_version_flag() {
    let workspace = unique_temp_dir("toolchain-prepare-plugin-no-version");
    seed_rust_toolchain_toml(
        &workspace,
        "[toolchain]\n\
         channel = \"1.94.1\"\n\
         \n\
         [soldr.plugins]\n\
         cargo-deny = \"*\"\n",
    );
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let rustup = install_logging_fake_rustup(&rustup_log);
    let cargo = install_logging_fake_cargo(&cargo_log);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["toolchain", "prepare"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .output()
        .expect("failed to run soldr toolchain prepare");

    assert!(
        output.status.success(),
        "soldr toolchain prepare failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let cargo_invocations = read_logged_cargo_invocations(&cargo_log);
    assert_eq!(
        cargo_invocations.len(),
        1,
        "expected exactly one cargo install: {cargo_invocations:?}"
    );
    let invocation = &cargo_invocations[0];
    assert_eq!(
        invocation,
        &vec!["install".to_string(), "cargo-deny".to_string()],
        "expected bare install argv (no --version for \"*\"): {invocation:?}"
    );
    assert!(
        !invocation.iter().any(|arg| arg == "--version"),
        "--version should be omitted when spec is \"*\": {invocation:?}"
    );
}
