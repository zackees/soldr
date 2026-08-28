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

/// emitted by the fake cargo script. Returns the `<value>` of the first match.
fn extract_linker_env_value(log: &str) -> Option<String> {
    extract_cargo_target_env_value(log, "LINKER")
}

fn extract_rustflags_env_value(log: &str) -> Option<String> {
    extract_cargo_target_env_value(log, "RUSTFLAGS")
}

fn extract_cargo_target_env_value(log: &str, suffix: &str) -> Option<String> {
    for line in log.lines() {
        let Some(rest) = line.strip_prefix("cargo_target_env ") else {
            continue;
        };
        let Some(eq_idx) = rest.find('=') else {
            continue;
        };
        let (name, value) = (&rest[..eq_idx], &rest[eq_idx + 1..]);
        if name.starts_with("CARGO_TARGET_") && name.ends_with(&format!("_{suffix}")) {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn log_has_any_cargo_target_env(log: &str) -> bool {
    log.lines()
        .any(|line| line.starts_with("cargo_target_env "))
}

#[test]
fn cargo_front_door_default_linker_does_not_inject_target_env() {
    let cache_root = unique_temp_dir("cargo-default-linker");
    let home_root = cache_root.join("home");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );
    let mut command = isolated_soldr_command();
    daemon.configure_client(&mut command);
    let output = command
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .env_remove("SOLDR_LINKER")
        .output()
        .expect("failed to run soldr cargo build with no SOLDR_LINKER");

    assert!(
        output.status.success(),
        "default-linker front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        !log_has_any_cargo_target_env(&log),
        "default linker should not inject any CARGO_TARGET_* env: {log}"
    );
}

#[test]
fn cargo_front_door_rust_lld_injects_target_linker_env() {
    let cache_root = unique_temp_dir("cargo-rust-lld-linker");
    let home_root = cache_root.join("home");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );
    let mut command = isolated_soldr_command();
    daemon.configure_client(&mut command);
    let output = command
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .env("SOLDR_LINKER", "rust-lld")
        .output()
        .expect("failed to run soldr cargo build with SOLDR_LINKER=rust-lld");

    assert!(
        output.status.success(),
        "rust-lld front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");

    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let linker_value = extract_linker_env_value(&log).unwrap_or_else(|| {
            panic!("expected CARGO_TARGET_<triple>_LINKER in fake cargo log: {log}")
        });
        let rustflags_value = extract_rustflags_env_value(&log);
        assert_eq!(
            linker_value, "rust-lld",
            "windows-msvc rust-lld should inject rust-lld directly: {log}"
        );
        assert!(
            rustflags_value.is_none(),
            "windows-msvc rust-lld should not inject rustflags: {log}"
        );
    } else if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::MacOs
    ) {
        // Issue #509: `SOLDR_LINKER=rust-lld` must not inject anything on
        // macOS — Apple clang rejects `-fuse-ld=lld`.
        assert!(
            !log_has_any_cargo_target_env(&log),
            "macOS rust-lld should not inject any CARGO_TARGET_* env (issue #509): {log}"
        );
    } else {
        let linker_value = extract_linker_env_value(&log).unwrap_or_else(|| {
            panic!("expected CARGO_TARGET_<triple>_LINKER in fake cargo log: {log}")
        });
        let rustflags_value = extract_rustflags_env_value(&log);
        assert_eq!(
            linker_value, "clang",
            "non-windows non-macos rust-lld should drive linking through clang: {log}"
        );
        assert_eq!(
            rustflags_value.as_deref(),
            Some("-C link-arg=-fuse-ld=lld"),
            "non-windows non-macos rust-lld should add -fuse-ld=lld rustflag: {log}"
        );
    }
}

#[test]
fn cargo_front_door_mold_on_non_linux_returns_clear_error() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Linux
    ) {
        return;
    }
    let cache_root = unique_temp_dir("cargo-mold-non-linux");
    let home_root = cache_root.join("home");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );
    let mut command = isolated_soldr_command();
    daemon.configure_client(&mut command);
    let output = command
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .env("SOLDR_LINKER", "mold")
        .output()
        .expect("failed to run soldr cargo build with SOLDR_LINKER=mold on non-linux");

    assert!(
        !output.status.success(),
        "SOLDR_LINKER=mold should fail on non-linux hosts; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mold is not supported"),
        "non-linux mold error message should mention 'mold is not supported': {stderr}"
    );
}

/// `SOLDR_LINKER=fast` resolution on non-Linux hosts:
///
/// - Windows MSVC injects `rust-lld` directly.
/// - macOS injects nothing (issue #509: Apple clang rejects
///   `-fuse-ld=lld`, so `fast` silently falls back to the platform
///   default linker).
///
/// The Linux variant of this matrix is exercised by the unit tests in
/// `crates/soldr-cli/src/linker.rs` (the `mold_present` probe is split
/// out for testability there). Gating to non-Linux here keeps the
/// integration test from depending on whether mold happens to be on
/// `PATH` on the CI runner.
#[test]
fn cargo_front_door_fast_picks_rust_lld_when_mold_absent() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Linux
    ) {
        return;
    }
    let cache_root = unique_temp_dir("cargo-fast-linker");
    let home_root = cache_root.join("home");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );
    let mut command = isolated_soldr_command();
    daemon.configure_client(&mut command);
    let output = command
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .env("SOLDR_LINKER", "fast")
        .output()
        .expect("failed to run soldr cargo build with SOLDR_LINKER=fast");

    assert!(
        output.status.success(),
        "fast-linker front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");

    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let linker_value = extract_linker_env_value(&log).unwrap_or_else(|| {
            panic!("expected CARGO_TARGET_<triple>_LINKER in fake cargo log: {log}")
        });
        assert_eq!(
            linker_value, "rust-lld",
            "windows-msvc fast should inject rust-lld directly: {log}"
        );
    } else if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::MacOs
    ) {
        // Issue #509: `SOLDR_LINKER=fast` must be a no-op on macOS so
        // Apple-clang-driven build scripts keep working.
        assert!(
            !log_has_any_cargo_target_env(&log),
            "macOS fast should not inject any CARGO_TARGET_* env (issue #509): {log}"
        );
    }
}
