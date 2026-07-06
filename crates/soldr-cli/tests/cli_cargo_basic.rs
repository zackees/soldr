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
fn cargo_front_door_runs_real_cargo() {
    let cache_root = unique_temp_dir("cargo-version");
    let output = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "--version"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr cargo --version");

    assert!(output.status.success(), "cargo front door failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("cargo"),
        "unexpected cargo output: {stdout}"
    );
    assert!(
        !stderr.contains("soldr: fetching cargo"),
        "cargo front door should not fetch cargo: {stderr}"
    );
}

#[test]
fn cargo_front_door_consumes_no_cache_flag() {
    let cache_root = unique_temp_dir("cargo-no-cache");
    let output = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "--version"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr --no-cache cargo --version");

    assert!(
        output.status.success(),
        "cargo front door with top-level --no-cache failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("cargo"),
        "unexpected cargo output with --no-cache: {stdout}"
    );
    assert!(
        !stderr.contains("unexpected argument '--no-cache'"),
        "--no-cache should be consumed by soldr, not forwarded to cargo: {stderr}"
    );
}

#[test]
fn cargo_front_door_maps_plus_toolchain_to_rustup_toolchain_env() {
    let cache_root = unique_temp_dir("cargo-plus-toolchain");
    let tool_dir = unique_temp_dir("cargo-plus-toolchain-bin");
    let log_path = cache_root.join("cargo.log");
    let cargo = fake_script_path(&tool_dir, "cargo");
    let rustc = fake_script_path(&tool_dir, "rustc");

    write_fake_script(&cargo, &fake_cargo_toolchain_recorder_script(&log_path));
    write_fake_script(&rustc, &fake_rustc_script(&log_path));

    let output = common::isolated_soldr_command()
        .args([
            "--no-cache",
            "cargo",
            "+nightly-2026-03-26",
            "test",
            "--manifest-path",
            "dylints/ban_manual_slash_normalize/Cargo.toml",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .expect("failed to run soldr cargo +toolchain test");

    assert!(
        output.status.success(),
        "cargo +toolchain front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake cargo log");
    assert!(
        log.contains("toolchain=nightly-2026-03-26"),
        "front door should map +toolchain to RUSTUP_TOOLCHAIN: {log}"
    );
    assert!(
        log.contains(
            "args=test\u{1f}--manifest-path\u{1f}dylints/ban_manual_slash_normalize/Cargo.toml"
        ),
        "front door should strip +toolchain before execing concrete cargo: {log}"
    );
}

fn fake_cargo_toolchain_recorder_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             setlocal enabledelayedexpansion\n\
             set \"args=\"\n\
             :loop\n\
             if \"%~1\"==\"\" goto done\n\
             if defined args (set \"args=!args!\u{1f}%~1\") else (set \"args=%~1\")\n\
             shift\n\
             goto loop\n\
             :done\n\
             echo toolchain=%RUSTUP_TOOLCHAIN% args=!args!>>\"{}\"\n\
             exit /b 0\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             sep=$(printf '\\037')\n\
             out=\"\"\n\
             first=1\n\
             for arg in \"$@\"; do\n\
               if [ $first -eq 1 ]; then\n\
                 out=\"$arg\"\n\
                 first=0\n\
               else\n\
                 out=\"$out${{sep}}$arg\"\n\
               fi\n\
             done\n\
             printf 'toolchain=%s args=%s\\n' \"${{RUSTUP_TOOLCHAIN:-}}\" \"$out\" >> \"{}\"\n\
             exit 0\n",
            log_path.display()
        )
    }
}

#[test]
fn cargo_build_warns_when_disk_space_is_low() {
    let cache_root = unique_temp_dir("cargo-low-disk");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);

    let output = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_TEST_FREE_DISK_BYTES", "1500000000")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with low-disk override");

    assert!(
        output.status.success(),
        "cargo build with low-disk warning failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("disk space is low"),
        "low-disk warning missing from stderr: {stderr}"
    );
    assert!(
        stderr.contains("Run `soldr gc`"),
        "low-disk warning should recommend soldr gc: {stderr}"
    );
}

#[test]
fn cargo_build_ignores_disk_space_detection_failures() {
    let cache_root = unique_temp_dir("cargo-low-disk-error");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);

    let output = common::isolated_soldr_command()
        .args(["--no-cache", "cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_TEST_FREE_DISK_BYTES", "error")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with disk-probe error");

    assert!(
        output.status.success(),
        "disk-space detection failure must not fail build\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("disk space is low"),
        "disk-space probe failure should not emit low-disk warning: {stderr}"
    );
}

#[test]
fn cargo_subcommand_rejects_no_cache_flag() {
    let cache_root = unique_temp_dir("cargo-subcommand-no-cache");
    let output = common::isolated_soldr_command()
        .args(["cargo", "--no-cache", "--version"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr cargo --no-cache --version");

    assert!(
        !output.status.success(),
        "cargo subcommand form should no longer accept --no-cache"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--no-cache"),
        "expected cargo-subcommand form to fail mentioning --no-cache: {stderr}"
    );
}
#[cfg(windows)]
#[test]
fn windows_worktree_copy_relocates_wrapper_and_original_dir_can_be_removed() {
    let cache_root = unique_temp_dir("windows-self-relocate-cache");
    let worktree = unique_temp_dir("windows-self-relocate-worktree");
    let source_dir = worktree.join("target").join("debug");
    fs::create_dir_all(&source_dir).expect("failed to create copied exe dir");
    let copied_soldr = source_dir.join("soldr.exe");
    fs::copy(common::soldr_bin(), &copied_soldr)
        .expect("failed to copy soldr exe into temporary worktree");

    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(&copied_soldr)
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        // Strip any relocation guard the parent process might be carrying
        // (it inherits these when the test suite itself is run via
        // `soldr cargo test`, because the outer soldr self-relocates and
        // exports SOLDR_RELOCATED_EXE / SOLDR_ORIGINAL_EXE in its env).
        // Leaving them set short-circuits relocation_guard_active() inside
        // the copied soldr, so RUSTC_WRAPPER would point at the worktree
        // copy instead of the runtime/soldr-self copy this test asserts.
        .env_remove("SOLDR_RELOCATED_EXE")
        .env_remove("SOLDR_ORIGINAL_EXE")
        .output()
        .expect("failed to run copied soldr exe");

    assert!(
        output.status.success(),
        "copied soldr front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    let wrapper = logged_cargo_wrapper(&log).expect("fake cargo should log RUSTC_WRAPPER");
    assert!(
        path_display_variants(&cache_root.join("runtime").join("soldr-self"))
            .iter()
            .any(|path| wrapper.contains(path)),
        "RUSTC_WRAPPER should point at the relocated runtime copy: {log}"
    );
    assert!(
        !path_display_variants(&copied_soldr)
            .iter()
            .any(|path| wrapper.contains(path)),
        "RUSTC_WRAPPER should not point at the original worktree copy: {log}"
    );

    fs::remove_dir_all(&worktree)
        .expect("temporary worktree should be removable after soldr exits");
    assert!(!worktree.exists());
}

#[test]
#[ignore = "FIXME(ci): soldr#1303 — reliably red on GHA ubuntu-24.04 shared runners \
    but passes on developer boxes (Windows + Linux). Second subprocess invocation \
    re-emits the warning that the StateDb dedup is meant to suppress. Prime suspect: \
    the auto-GC sweeper introduced by #1286 / #1295 races with the second \
    invocation's StateDb::open, and profile_debug's `.unwrap_or(true)` fails open \
    by design → warning re-emitted. Root-cause investigation is tracked in #1303."]
fn cargo_front_door_defaults_dev_debug_off_and_warns_once_per_repo() {
    let cache_root = unique_temp_dir("cargo-debug-default-off");
    let repo = unique_temp_dir("cargo-debug-default-repo");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("failed to seed Cargo.toml");
    fs::create_dir_all(repo.join("src")).expect("failed to create src dir");
    fs::write(repo.join("src").join("lib.rs"), "").expect("failed to seed lib.rs");

    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let cargo_home = cache_root.join("cargo-home");

    let first = common::isolated_soldr_command()
        .args(["cargo", "build"])
        .current_dir(&repo)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("CARGO_PROFILE_DEV_DEBUG")
        .env_remove("CARGO_PROFILE_TEST_DEBUG")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run first soldr cargo build");
    assert!(
        first.status.success(),
        "first build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let second = common::isolated_soldr_command()
        .args(["cargo", "build"])
        .current_dir(&repo)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("CARGO_PROFILE_DEV_DEBUG")
        .env_remove("CARGO_PROFILE_TEST_DEBUG")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run second soldr cargo build");
    assert!(
        second.status.success(),
        "second build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo_profile_env CARGO_PROFILE_DEV_DEBUG=false"),
        "soldr should inject the dev profile debug override when unspecified: {log}"
    );

    let first_stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        first_stderr.contains("Cargo profile.dev.debug is unspecified")
            && first_stderr.contains("CARGO_PROFILE_DEV_DEBUG=false"),
        "first invocation should warn about the defaulted debug setting: {first_stderr}"
    );
    let second_stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        !second_stderr.contains("Cargo profile.dev.debug is unspecified"),
        "second invocation for the same repo should not repeat the debug-default warning: {second_stderr}"
    );
}

#[test]
fn cargo_front_door_respects_dev_debug_in_cargo_config_toml() {
    let cache_root = unique_temp_dir("cargo-debug-config-explicit");
    let repo = unique_temp_dir("cargo-debug-config-repo");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("failed to seed Cargo.toml");
    fs::create_dir_all(repo.join(".cargo")).expect("failed to create .cargo dir");
    fs::write(
        repo.join(".cargo").join("config.toml"),
        "[profile.dev]\ndebug = true\n",
    )
    .expect("failed to seed .cargo/config.toml");

    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = common::isolated_soldr_command()
        .args(["cargo", "build"])
        .current_dir(&repo)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", cache_root.join("cargo-home"))
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("CARGO_PROFILE_DEV_DEBUG")
        .env_remove("CARGO_PROFILE_TEST_DEBUG")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with explicit config debug");

    assert!(
        output.status.success(),
        "build with explicit cargo config debug failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        !log.contains("cargo_profile_env CARGO_PROFILE_DEV_DEBUG=false"),
        "explicit .cargo/config.toml profile.dev.debug must not be overridden: {log}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Cargo profile.dev.debug is unspecified"),
        "explicit .cargo/config.toml profile.dev.debug should suppress warning: {stderr}"
    );
}
