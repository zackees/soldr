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
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
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
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
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
    let linker_value = extract_linker_env_value(&log).unwrap_or_else(|| {
        panic!("expected CARGO_TARGET_<triple>_LINKER in fake cargo log: {log}")
    });
    let rustflags_value = extract_rustflags_env_value(&log);

    #[cfg(target_os = "windows")]
    {
        assert_eq!(
            linker_value, "rust-lld",
            "windows-msvc rust-lld should inject rust-lld directly: {log}"
        );
        assert!(
            rustflags_value.is_none(),
            "windows-msvc rust-lld should not inject rustflags: {log}"
        );
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(
            linker_value, "clang",
            "non-windows rust-lld should drive linking through clang: {log}"
        );
        assert_eq!(
            rustflags_value.as_deref(),
            Some("-C link-arg=-fuse-ld=lld"),
            "non-windows rust-lld should add -fuse-ld=lld rustflag: {log}"
        );
    }
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn cargo_front_door_mold_on_non_linux_returns_clear_error() {
    let cache_root = unique_temp_dir("cargo-mold-non-linux");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
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

/// `SOLDR_LINKER=fast` picks `rust-lld` on every non-Linux host. The
/// Linux variant of this matrix is exercised by the unit tests in
/// `crates/soldr-cli/src/linker.rs` (the `mold_present` probe is split
/// out for testability there). Gating to non-Linux here keeps the
/// integration test from depending on whether mold happens to be on
/// `PATH` on the CI runner.
#[cfg(not(target_os = "linux"))]
#[test]
fn cargo_front_door_fast_picks_rust_lld_when_mold_absent() {
    let cache_root = unique_temp_dir("cargo-fast-linker");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
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
    let linker_value = extract_linker_env_value(&log).unwrap_or_else(|| {
        panic!("expected CARGO_TARGET_<triple>_LINKER in fake cargo log: {log}")
    });

    #[cfg(target_os = "windows")]
    {
        assert_eq!(
            linker_value, "rust-lld",
            "windows-msvc fast should inject rust-lld directly: {log}"
        );
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(
            linker_value, "clang",
            "macOS fast should drive linking through clang: {log}"
        );
        assert_eq!(
            extract_rustflags_env_value(&log).as_deref(),
            Some("-C link-arg=-fuse-ld=lld"),
            "macOS fast should add -fuse-ld=lld rustflag: {log}"
        );
    }
}
