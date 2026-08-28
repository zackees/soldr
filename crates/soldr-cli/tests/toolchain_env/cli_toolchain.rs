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

fn fake_toolchain_plugin_cargo_script(log_path: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} workspace_wrapper=${{RUSTC_WORKSPACE_WRAPPER:-}} args=$*\" >> \"{}\"\n",
        log_path.display()
    )
}

fn install_fake_toolchain_plugin_cargo(log_path: &Path) -> PathBuf {
    let dir = unique_temp_dir("fake-toolchain-plugin-cargo");
    let cargo = fake_script_path(&dir, "cargo");
    write_fake_script(&cargo, &fake_toolchain_plugin_cargo_script(log_path));
    cargo
}

fn fake_rustup_panic_script() -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "@echo off\necho fake rustup panic: component target should be known 1>&2\nexit /b 101\n"
            .to_string()
    } else {
        "#!/bin/sh\nprintf '%s\\n' 'fake rustup panic: component target should be known' >&2\nexit 101\n"
            .to_string()
    }
}

#[test]
fn rustup_passthrough_preserves_a_failing_child_diagnostic() {
    let workspace = unique_temp_dir("rustup-passthrough-panic");
    let tools = workspace.join("tools");
    fs::create_dir_all(&tools).expect("tool directory");
    let rustup = fake_script_path(&tools, "rustup");
    write_fake_script(&rustup, &fake_rustup_panic_script());

    let output = isolated_soldr_command()
        .args(["rustup", "show"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .output()
        .expect("run soldr rustup passthrough");

    assert_eq!(
        output.status.code(),
        Some(101),
        "child status must propagate"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("fake rustup panic"),
        "child stderr was lost: {stderr}"
    );
    assert!(
        !stderr.contains("soldr emitted no diagnostic and ran no child process"),
        "a child diagnostic must not be followed by Soldr's false fault annotation: {stderr}"
    );
}

// Given a 180s nextest budget (see `.config/nextest.toml`) as a smoke proof that
// the watchdog macro composes cleanly with an existing integration
// test that spawns the soldr binary as a subprocess.
#[test]
fn rustup_passthrough_forwards_args_unchanged_for_unscoped_subcommands() {
    let workspace = unique_temp_dir("rustup-passthrough-show");
    let log_path = workspace.join("rustup.log");
    let rustup = install_logging_fake_rustup(&log_path);

    let output = isolated_soldr_command()
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

    let output = isolated_soldr_command()
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

    let output = isolated_soldr_command()
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

    let output = isolated_soldr_command()
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

    let output = isolated_soldr_command()
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

    let output = isolated_soldr_command()
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
fn toolchain_prepare_host_cargo_keeps_managed_install_home_without_managed_rustup_home() {
    let workspace = unique_temp_dir("toolchain-prepare-plugin-home-boundary");
    seed_rust_toolchain_toml(
        &workspace,
        "[toolchain]\n\
         channel = \"1.94.1\"\n\
         \n\
         [soldr.plugins]\n\
         cargo-nextest = \"0.9\"\n",
    );
    let cache_root = unique_temp_dir("toolchain-prepare-plugin-managed-home");
    let managed_cargo_home = cache_root.join("cargo");
    let managed_rustup_home = cache_root.join("rustup");
    fs::create_dir_all(&managed_cargo_home).expect("create managed Cargo home");
    fs::create_dir_all(&managed_rustup_home).expect("create managed Rustup home");

    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let rustup = install_logging_fake_rustup(&rustup_log);
    let host_tool_dir = unique_temp_dir("toolchain-prepare-plugin-host-tools");
    let (cargo, _, _) = install_fake_version_toolchain(&host_tool_dir, &cargo_log);

    let output = isolated_soldr_command()
        .args(["toolchain", "prepare"])
        .current_dir(&workspace)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .output()
        .expect("failed to run soldr toolchain prepare");

    assert!(
        output.status.success(),
        "soldr toolchain prepare failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&cargo_log).expect("read fake cargo log");
    let cargo_line = log
        .lines()
        .find(|line| line.starts_with("cargo "))
        .unwrap_or_else(|| panic!("plugin install did not invoke host Cargo: {log}"));
    assert!(
        path_display_variants(&managed_cargo_home)
            .iter()
            .any(|path| cargo_line.contains(&format!("cargo_home={path}"))),
        "plugin install must keep Soldr's managed CARGO_HOME: {cargo_line}"
    );
    assert!(
        path_display_variants(&managed_rustup_home)
            .iter()
            .all(|path| !cargo_line.contains(&format!("rustup_home={path}"))),
        "host Cargo must not receive Soldr's managed RUSTUP_HOME: {cargo_line}"
    );
}

#[test]
fn toolchain_prepare_plugin_install_clears_inherited_rustc_wrappers() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let workspace = unique_temp_dir("toolchain-prepare-plugin-wrapper-policy");
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
    let cargo = install_fake_toolchain_plugin_cargo(&cargo_log);

    let output = isolated_soldr_command()
        .args(["toolchain", "prepare"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("RUSTC_WRAPPER", "/tmp/outer-wrapper")
        .env("RUSTC_WORKSPACE_WRAPPER", "/tmp/outer-workspace-wrapper")
        .output()
        .expect("failed to run soldr toolchain prepare");

    assert!(
        output.status.success(),
        "soldr toolchain prepare failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&cargo_log).expect("read fake cargo log");
    assert!(
        log.contains("cargo wrapper= workspace_wrapper= args=install cargo-nextest --version 0.9"),
        "toolchain prepare plugin install should scrub rustc wrapper env: {log}"
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

    let output = isolated_soldr_command()
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
        "expected in argv: {invocation:?}"
    );
}

#[test]
fn rustup_passthrough_injects_toolchain_for_component_add() {
    let workspace = unique_temp_dir("rustup-passthrough-component-add");
    seed_rust_toolchain_toml(&workspace, "[toolchain]\nchannel = \"1.94.1\"\n");
    let log_path = workspace.join("rustup.log");
    let rustup = install_logging_fake_rustup(&log_path);

    let output = isolated_soldr_command()
        .args(["rustup", "component", "add", "clippy"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .output()
        .expect("failed to run soldr rustup component add");

    assert!(
        output.status.success(),
        "soldr rustup component add failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let invocations = read_logged_rustup_invocations(&log_path);
    assert_eq!(invocations.len(), 1, "expected one rustup invocation");
    assert_eq!(
        invocations[0],
        vec![
            "component".to_string(),
            "add".to_string(),
            "--toolchain".to_string(),
            "1.94.1".to_string(),
            "clippy".to_string(),
        ],
    );
}

#[test]
fn rustup_passthrough_does_not_inject_for_toolchain_list() {
    // `rustup toolchain list` is a top-level mgmt verb, not a per-toolchain
    // mutation. Scoping injection rules out injecting `--toolchain` here.
    let workspace = unique_temp_dir("rustup-passthrough-toolchain-list");
    seed_rust_toolchain_toml(&workspace, "[toolchain]\nchannel = \"1.94.1\"\n");
    let log_path = workspace.join("rustup.log");
    let rustup = install_logging_fake_rustup(&log_path);

    let output = isolated_soldr_command()
        .args(["rustup", "toolchain", "list"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .output()
        .expect("failed to run soldr rustup toolchain list");

    assert!(output.status.success());
    let invocations = read_logged_rustup_invocations(&log_path);
    assert_eq!(invocations.len(), 1);
    assert_eq!(
        invocations[0],
        vec!["toolchain".to_string(), "list".to_string()],
        "verbatim passthrough (no --toolchain injection)",
    );
}

#[test]
fn rustup_passthrough_forwards_version_flag_verbatim() {
    let workspace = unique_temp_dir("rustup-passthrough-version-flag");
    seed_rust_toolchain_toml(&workspace, "[toolchain]\nchannel = \"1.94.1\"\n");
    let log_path = workspace.join("rustup.log");
    let rustup = install_logging_fake_rustup(&log_path);

    let output = isolated_soldr_command()
        .args(["rustup", "--version"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .output()
        .expect("failed to run soldr rustup --version");

    assert!(output.status.success());
    let invocations = read_logged_rustup_invocations(&log_path);
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0], vec!["--version".to_string()]);
}

#[test]
fn rustup_passthrough_handles_target_add_equals_form_for_explicit_toolchain() {
    // `--toolchain=<value>` (= form) must suppress injection just like
    // the space-separated form (`--toolchain <value>`).
    let workspace = unique_temp_dir("rustup-passthrough-target-add-equals");
    seed_rust_toolchain_toml(&workspace, "[toolchain]\nchannel = \"1.94.1\"\n");
    let log_path = workspace.join("rustup.log");
    let rustup = install_logging_fake_rustup(&log_path);

    let output = isolated_soldr_command()
        .args([
            "rustup",
            "target",
            "add",
            "--toolchain=nightly",
            "aarch64-apple-darwin",
        ])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .output()
        .expect("failed to run soldr rustup target add --toolchain=...");

    assert!(output.status.success());
    let invocations = read_logged_rustup_invocations(&log_path);
    assert_eq!(invocations.len(), 1);
    let invocation = &invocations[0];
    let bare_count = invocation.iter().filter(|a| *a == "--toolchain").count();
    let equals_count = invocation
        .iter()
        .filter(|a| a.starts_with("--toolchain="))
        .count();
    assert_eq!(
        bare_count + equals_count,
        1,
        "--toolchain should appear exactly once (any form): {invocation:?}"
    );
    assert!(
        invocation.iter().any(|a| a == "--toolchain=nightly"),
        "user-supplied --toolchain=nightly should be preserved: {invocation:?}"
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

    let output = isolated_soldr_command()
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

// ===========================================================================
// Tests for `soldr toolchain ensure` — issue #407 Phase 2.
//
// `ensure` is `prepare` + a smoke verify (`cargo --version` / `rustc
// --version`), with an optional `--json` output that setup-soldr will
// consume. The schema is locked at version 1; bumping it requires a
// version bump in `ToolchainEnsureOutput` AND in the consumers.
// ===========================================================================

#[test]
fn toolchain_ensure_runs_prepare_then_smoke_verify_in_json_mode() {
    let workspace = unique_temp_dir("toolchain-ensure-json");
    seed_rust_toolchain_toml(
        &workspace,
        "[toolchain]\n\
         channel = \"1.94.1\"\n\
         components = [\"clippy\", \"rustfmt\"]\n\
         targets = [\"x86_64-unknown-linux-musl\"]\n\
         \n\
         [soldr.plugins]\n\
         cargo-nextest = \"0.9\"\n",
    );
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let rustup = install_logging_fake_rustup(&rustup_log);
    let cargo =
        install_logging_versioned_fake_cargo(&cargo_log, "cargo 1.94.1 (abc1234 2026-04-15)");
    let rustc = install_versioned_fake_rustc("rustc 1.94.1 (def5678 2026-04-15)");

    let output = isolated_soldr_command()
        .args(["toolchain", "ensure", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .output()
        .expect("failed to run soldr toolchain ensure --json");

    assert!(
        output.status.success(),
        "soldr toolchain ensure --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("ensure --json stdout not JSON: {stdout}"));

    // Schema version is part of the contract setup-soldr#133 consumes.
    assert_eq!(parsed["schema_version"], Value::from(1));
    assert_eq!(parsed["channel"], Value::from("1.94.1"));
    assert_eq!(parsed["rustup_bootstrapped"], Value::from(false));

    // Components / targets / plugins MUST be present as arrays even when
    // empty so consumers can index unconditionally.
    let components = parsed["components_added"]
        .as_array()
        .expect("components_added missing or not array");
    let component_strs: Vec<&str> = components
        .iter()
        .map(|v| v.as_str().expect("component not string"))
        .collect();
    assert!(component_strs.contains(&"clippy"));
    assert!(component_strs.contains(&"rustfmt"));

    let targets = parsed["targets_added"]
        .as_array()
        .expect("targets_added missing or not array");
    let target_strs: Vec<&str> = targets
        .iter()
        .map(|v| v.as_str().expect("target not string"))
        .collect();
    assert_eq!(target_strs, vec!["x86_64-unknown-linux-musl"]);

    let plugins = parsed["plugins_installed"]
        .as_array()
        .expect("plugins_installed missing or not array");
    let plugin_strs: Vec<&str> = plugins
        .iter()
        .map(|v| v.as_str().expect("plugin not string"))
        .collect();
    assert_eq!(plugin_strs, vec!["cargo-nextest@0.9"]);

    let smoke = &parsed["smoke_verify"];
    assert_eq!(smoke["ok"], Value::from(true));
    assert_eq!(
        smoke["cargo_version"].as_str().unwrap_or(""),
        "cargo 1.94.1 (abc1234 2026-04-15)"
    );
    assert_eq!(
        smoke["rustc_version"].as_str().unwrap_or(""),
        "rustc 1.94.1 (def5678 2026-04-15)"
    );

    // elapsed_ms is non-deterministic, but must exist and be a number.
    assert!(
        parsed["elapsed_ms"].is_number(),
        "elapsed_ms must be numeric"
    );

    // Verify the rustup invocations are the same set `prepare` would run:
    // install + 2 components + 1 target.
    let rustup_invocations = read_logged_rustup_invocations(&rustup_log);
    assert_eq!(
        rustup_invocations.len(),
        4,
        "expected install + 2 components + 1 target rustup invocations: {rustup_invocations:?}"
    );
    assert_eq!(rustup_invocations[0][0], "toolchain");
    assert_eq!(rustup_invocations[0][1], "install");

    // Verify cargo was called once for the plugin AND once for --version
    // (smoke verify). Both land in the same log.
    let cargo_invocations = read_logged_cargo_invocations(&cargo_log);
    let install_invocations: Vec<_> = cargo_invocations
        .iter()
        .filter(|inv| inv.first().map(String::as_str) == Some("install"))
        .collect();
    assert_eq!(
        install_invocations.len(),
        1,
        "expected exactly one `cargo install` invocation for the plugin: {cargo_invocations:?}"
    );
    assert_eq!(
        install_invocations[0].get(1).map(String::as_str),
        Some("cargo-nextest")
    );
}

#[test]
fn toolchain_ensure_human_mode_succeeds_without_json() {
    let workspace = unique_temp_dir("toolchain-ensure-human");
    seed_rust_toolchain_toml(&workspace, "[toolchain]\nchannel = \"1.94.1\"\n");
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let rustup = install_logging_fake_rustup(&rustup_log);
    let cargo =
        install_logging_versioned_fake_cargo(&cargo_log, "cargo 1.94.1 (abc1234 2026-04-15)");
    let rustc = install_versioned_fake_rustc("rustc 1.94.1 (def5678 2026-04-15)");

    let output = isolated_soldr_command()
        .args(["toolchain", "ensure"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .output()
        .expect("failed to run soldr toolchain ensure");

    assert!(
        output.status.success(),
        "soldr toolchain ensure (human mode) failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Human mode MUST NOT emit raw JSON on stdout.
    assert!(
        !stdout.trim_start().starts_with('{'),
        "human-mode output must not be JSON: {stdout}"
    );

    // Sanity: the user-facing line mentions the resolved channel.
    assert!(
        stdout.contains("1.94.1"),
        "expected channel in human output: {stdout}"
    );
}

#[test]
fn toolchain_ensure_json_reports_smoke_failure_when_rustc_returns_nonzero() {
    let workspace = unique_temp_dir("toolchain-ensure-smoke-fail");
    seed_rust_toolchain_toml(&workspace, "[toolchain]\nchannel = \"1.94.1\"\n");
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let rustup = install_logging_fake_rustup(&rustup_log);
    let cargo =
        install_logging_versioned_fake_cargo(&cargo_log, "cargo 1.94.1 (abc1234 2026-04-15)");
    let rustc = install_failing_fake_rustc();

    let output = isolated_soldr_command()
        .args(["toolchain", "ensure", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .output()
        .expect("failed to run soldr toolchain ensure --json (failing smoke)");

    // Smoke failure is a soldr-level failure: the JSON must still be
    // emitted (so callers can inspect it) but the exit code must be
    // non-zero so shell scripts notice. setup-soldr#133 relies on this.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("ensure --json stdout not JSON: {stdout}"));
    assert_eq!(parsed["schema_version"], Value::from(1));
    let smoke = &parsed["smoke_verify"];
    assert_eq!(
        smoke["ok"],
        Value::from(false),
        "smoke_verify.ok must be false when rustc --version fails: {smoke}"
    );
    assert!(
        !output.status.success(),
        "ensure must exit non-zero when smoke verify fails (status={:?})",
        output.status
    );
}

#[test]
fn toolchain_ensure_no_channel_emits_empty_schema_v1_payload() {
    let workspace = unique_temp_dir("toolchain-ensure-no-channel");
    // No rust-toolchain.toml at all: ensure must still emit valid JSON
    // (schema_version=1, channel=null, no smoke verify) and exit 0.
    let rustup_log = workspace.join("rustup.log");
    let cargo_log = workspace.join("cargo.log");
    let rustup = install_logging_fake_rustup(&rustup_log);
    let cargo =
        install_logging_versioned_fake_cargo(&cargo_log, "cargo 1.94.1 (abc1234 2026-04-15)");
    let rustc = install_versioned_fake_rustc("rustc 1.94.1 (def5678 2026-04-15)");

    let output = isolated_soldr_command()
        .args(["toolchain", "ensure", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .output()
        .expect("failed to run soldr toolchain ensure --json (no channel)");

    assert!(
        output.status.success(),
        "ensure with no manifest must exit 0\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("ensure --json stdout not JSON: {stdout}"));
    assert_eq!(parsed["schema_version"], Value::from(1));
    assert!(
        parsed["channel"].is_null(),
        "channel must be null when no manifest: {parsed}"
    );
}

// ===========================================================================
// Tests for `soldr toolchain link --shim-dir <path>` — issue #407 Phase 3.
//
// `link` writes platform-specific shim files (cargo, rustfmt,
// clippy-driver, rustc, rustdoc) into <shim-dir> so PATH-resolved
// invocations of those tools route back through `soldr <tool>`. The
// `--json` payload follows the same `schema_version: 1` style as
// `ensure` so setup-soldr#133 can consume both with one parser.
// ===========================================================================

/// Platform-aware shim filename. Mirrors `toolchain_link::shim_path` so
/// the integration tests don't have to depend on the bin-tree module.
/// Shims are native multicall executables, so Windows gets `<tool>.exe`.
fn expected_shim_path(dir: &Path, tool: &str) -> PathBuf {
    dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX))
}

fn assert_native_shim_matches_soldr(shim: &Path, soldr_bin: &Path, tool: &str) {
    let shim_bytes =
        fs::read(shim).unwrap_or_else(|err| panic!("read {tool} shim {}: {err}", shim.display()));
    let soldr_bytes = fs::read(soldr_bin)
        .unwrap_or_else(|err| panic!("read soldr binary {}: {err}", soldr_bin.display()));
    assert_eq!(
        shim_bytes,
        soldr_bytes,
        "{tool} shim should be a native multicall copy/hardlink of {}",
        soldr_bin.display()
    );
}

fn expected_toolchain_link_source_soldr_bin() -> PathBuf {
    // `isolated_soldr_command` deliberately scrubs the outer dogfooding
    // process's relocation markers. The child therefore links from the
    // test-built soldr binary, regardless of which installed soldr launched
    // the outer test command.
    common::soldr_bin()
}

#[test]
fn toolchain_link_writes_every_routed_tool_into_shim_dir() {
    let workspace = unique_temp_dir("toolchain-link-fresh");
    let shim_dir = workspace.join("shims");

    let output = isolated_soldr_command()
        .args([
            "toolchain",
            "link",
            "--shim-dir",
            &shim_dir.display().to_string(),
        ])
        .current_dir(&workspace)
        .output()
        .expect("failed to run soldr toolchain link");

    assert!(
        output.status.success(),
        "soldr toolchain link failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let soldr_bin = expected_toolchain_link_source_soldr_bin();
    // Shims are native multicall hardlinks/copies of the running soldr
    // binary (issue #1302), so each shim must be byte-identical to the
    // soldr binary - that's what guarantees subprocess exec lands back
    // on this build.
    for tool in ["cargo", "rustfmt", "clippy-driver", "rustc", "rustdoc"] {
        let shim = expected_shim_path(&shim_dir, tool);
        assert!(
            shim.is_file(),
            "expected shim at {} after link, but it's missing",
            shim.display()
        );
        assert_native_shim_matches_soldr(&shim, &soldr_bin, tool);
    }
}

#[test]
fn toolchain_link_emits_schema_v1_json_payload() {
    let workspace = unique_temp_dir("toolchain-link-json");
    let shim_dir = workspace.join("shims");

    let output = isolated_soldr_command()
        .args([
            "toolchain",
            "link",
            "--shim-dir",
            &shim_dir.display().to_string(),
            "--json",
        ])
        .current_dir(&workspace)
        .output()
        .expect("failed to run soldr toolchain link --json");

    assert!(
        output.status.success(),
        "soldr toolchain link --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("link --json stdout not JSON: {stdout}"));

    assert_eq!(parsed["schema_version"], Value::from(1));
    assert!(parsed["shim_dir"].is_string(), "shim_dir must be a string");
    assert!(
        parsed["elapsed_ms"].is_number(),
        "elapsed_ms must be a number"
    );

    let tools = parsed["tools"].as_array().expect("tools must be an array");
    assert_eq!(
        tools.len(),
        5,
        "expected entries for the 5 routed tools, got: {tools:?}"
    );

    // Order is part of the contract — setup-soldr#133 consumers index
    // by name but `cargo` should be first for human-readable output.
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("name string"))
        .collect();
    assert_eq!(
        names,
        vec!["cargo", "rustfmt", "clippy-driver", "rustc", "rustdoc"]
    );

    for entry in tools {
        assert_eq!(entry["created"], Value::from(true));
        assert!(
            entry.get("skip_reason").is_none(),
            "fresh entry must not carry skip_reason: {entry}"
        );
    }
}

#[test]
fn toolchain_link_is_idempotent_when_rerun_with_same_soldr_binary() {
    let workspace = unique_temp_dir("toolchain-link-idempotent");
    let shim_dir = workspace.join("shims");

    let first = isolated_soldr_command()
        .args([
            "toolchain",
            "link",
            "--shim-dir",
            &shim_dir.display().to_string(),
            "--json",
        ])
        .current_dir(&workspace)
        .output()
        .expect("first link");
    assert!(first.status.success(), "first link must succeed");

    // Capture mtimes after the first run so we can verify the second
    // run does NOT touch the files.
    let mtimes_before: Vec<SystemTime> = ["cargo", "rustfmt", "clippy-driver", "rustc", "rustdoc"]
        .iter()
        .map(|tool| {
            let path = expected_shim_path(&shim_dir, tool);
            fs::metadata(&path)
                .expect("stat first run")
                .modified()
                .expect("mtime")
        })
        .collect();

    // Tiny delay so a re-write would show up as a strictly newer mtime
    // on filesystems with low-res mtimes.
    std::thread::sleep(Duration::from_millis(50));

    let second = isolated_soldr_command()
        .args([
            "toolchain",
            "link",
            "--shim-dir",
            &shim_dir.display().to_string(),
            "--json",
        ])
        .current_dir(&workspace)
        .output()
        .expect("second link");
    assert!(second.status.success(), "second link must succeed");

    let stdout = String::from_utf8_lossy(&second.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("second link --json stdout not JSON: {stdout}"));
    let tools = parsed["tools"].as_array().expect("tools array");
    for entry in tools {
        assert_eq!(
            entry["created"],
            Value::from(false),
            "second run must not re-create: {entry}"
        );
        assert_eq!(
            entry["skip_reason"].as_str().unwrap_or(""),
            "existing-matches",
            "second run must report existing-matches: {entry}"
        );
    }

    for (tool, before) in ["cargo", "rustfmt", "clippy-driver", "rustc", "rustdoc"]
        .iter()
        .zip(mtimes_before)
    {
        let path = expected_shim_path(&shim_dir, tool);
        let after = fs::metadata(&path)
            .expect("stat second run")
            .modified()
            .expect("mtime");
        assert_eq!(
            after,
            before,
            "shim file for {tool} was rewritten despite matching content: {}",
            path.display()
        );
    }
}

#[test]
fn toolchain_link_force_overwrites_user_modified_shim() {
    let workspace = unique_temp_dir("toolchain-link-force");
    let shim_dir = workspace.join("shims");
    fs::create_dir_all(&shim_dir).expect("mkdir shim dir");

    // Seed every shim with foreign content (e.g. a user-customized shim
    // we are about to clobber).
    for tool in ["cargo", "rustfmt", "clippy-driver", "rustc", "rustdoc"] {
        let path = expected_shim_path(&shim_dir, tool);
        fs::write(&path, "USER CUSTOM").expect("seed user shim");
    }

    // Without --force the run must NOT overwrite differing content.
    let no_force = isolated_soldr_command()
        .args([
            "toolchain",
            "link",
            "--shim-dir",
            &shim_dir.display().to_string(),
            "--json",
        ])
        .current_dir(&workspace)
        .output()
        .expect("no-force link");
    assert!(no_force.status.success());
    for tool in ["cargo", "rustfmt", "clippy-driver", "rustc", "rustdoc"] {
        let path = expected_shim_path(&shim_dir, tool);
        let body = fs::read_to_string(&path).expect("read shim");
        assert_eq!(
            body, "USER CUSTOM",
            "no-force link must leave user-customized {tool} alone (body={body:?})"
        );
    }

    // With --force the run MUST overwrite.
    let with_force = isolated_soldr_command()
        .args([
            "toolchain",
            "link",
            "--shim-dir",
            &shim_dir.display().to_string(),
            "--force",
            "--json",
        ])
        .current_dir(&workspace)
        .output()
        .expect("force link");
    assert!(with_force.status.success());

    let stdout = String::from_utf8_lossy(&with_force.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("force link --json stdout not JSON: {stdout}"));
    let tools = parsed["tools"].as_array().expect("tools array");
    for entry in tools {
        assert_eq!(
            entry["created"],
            Value::from(true),
            "--force must overwrite differing shim: {entry}"
        );
    }
    let soldr_bin = expected_toolchain_link_source_soldr_bin();
    for tool in ["cargo", "rustfmt", "clippy-driver", "rustc", "rustdoc"] {
        let path = expected_shim_path(&shim_dir, tool);
        let body = fs::read(&path).expect("read shim");
        assert_ne!(
            body,
            b"USER CUSTOM".to_vec(),
            "--force must overwrite {tool} shim (still USER CUSTOM)"
        );
        assert_native_shim_matches_soldr(&path, &soldr_bin, tool);
    }
}
