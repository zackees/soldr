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
fn cargo_front_door_uses_real_tool_overrides_before_path_probe() {
    let cache_root = unique_temp_dir("cargo-real-tool-overrides");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let shim_dir = unique_temp_dir("cargo-shim-dir");
    let shim_cargo = fake_script_path(&shim_dir, "cargo");
    write_fake_script(
        &shim_cargo,
        &fake_version_tool_script(&log_path, "shim-cargo"),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_REAL_CARGO", &cargo)
        .env("SOLDR_REAL_RUSTC", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("PATH", prepend_to_path(&shim_dir))
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with real tool overrides");

    assert!(
        output.status.success(),
        "real-tool override front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo wrapper="),
        "real cargo should have been invoked: {log}"
    );
    assert!(
        !log.contains("shim-cargo"),
        "PATH shim should not be resolved when SOLDR_REAL_CARGO is set: {log}"
    );
}

#[test]
fn cargo_front_door_does_not_start_cache_for_non_build_subcommands() {
    let cache_root = unique_temp_dir("cargo-non-build-no-cache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "metadata"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cargo metadata with fake tools");

    assert!(
        output.status.success(),
        "non-build cargo front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cache=0"),
        "non-build cargo commands should propagate cache disabled: {log}"
    );
    assert!(
        !log.contains("zccache start")
            && !log.contains("zccache session-start")
            && !log.contains("zccache wrapper")
            && !log.contains("zccache session-end"),
        "managed zccache should be skipped for non-build cargo commands: {log}"
    );
}

#[test]
fn cargo_front_door_detects_build_after_global_cargo_options() {
    let cache_root = unique_temp_dir("cargo-global-options-cache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "--manifest-path", "demo/Cargo.toml", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with global cargo options");

    assert!(
        output.status.success(),
        "global-option cargo front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cache=1") && log.contains("zccache start"),
        "build after global cargo options should still use managed zccache: {log}"
    );
}

#[cfg(not(windows))]
#[test]
fn cargo_front_door_preserves_jobserver_fds_into_managed_zccache_wrapper() {
    let cache_root = unique_temp_dir("cargo-jobserver-fds");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_jobserver_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "test", "--no-run"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo test --no-run with fake jobserver fds");

    assert!(
        output.status.success(),
        "cache-enabled front door lost jobserver fds\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("failed to connect to jobserver"),
        "jobserver warning should not be emitted: {stderr}"
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("zccache jobserver fds ok read=3 write=4"),
        "managed zccache wrapper did not observe open jobserver fds: {log}"
    );
}

#[test]
fn cache_enabled_zccache_build_completes_under_20_seconds() {
    let cache_root = unique_temp_dir("cargo-zccache-timing");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);

    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with fake zccache");
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "cache-enabled zccache build failed in {elapsed:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "cache-enabled zccache build took {elapsed:?}, expected under 20s"
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("zccache start")
            && log.contains("zccache session-start")
            && log.contains("zccache wrapper")
            && log.contains("zccache session-end test-session"),
        "timed build should exercise the managed zccache path: {log}"
    );
}

#[test]
fn managed_zccache_rejects_conflicting_cache_dir_override() {
    let cache_root = unique_temp_dir("cargo-conflicting-zccache-dir");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("ZCCACHE_CACHE_DIR", cache_root.join("user-zccache"))
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cargo build with conflicting ZCCACHE_CACHE_DIR");

    assert!(
        !output.status.success(),
        "conflicting ZCCACHE_CACHE_DIR should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ZCCACHE_CACHE_DIR is managed by soldr"),
        "expected explicit override guidance: {stderr}"
    );
    assert!(
        !log_path.exists(),
        "zccache should not start after a conflicting cache-dir override"
    );
}

#[test]
fn nested_soldr_ignores_inherited_managed_zccache_cache_dir() {
    let parent_cache_root = unique_temp_dir("cargo-parent-managed-zccache-dir");
    let child_cache_root = unique_temp_dir("cargo-child-managed-zccache-dir");
    let parent_zccache_dir = parent_cache_root.join("cache").join("zccache");
    let child_zccache_dir = child_cache_root.join("cache").join("zccache");
    let log_path = child_cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &child_cache_root)
        .env("ZCCACHE_CACHE_DIR", &parent_zccache_dir)
        .env("SOLDR_MANAGED_ZCCACHE_CACHE_DIR", &parent_zccache_dir)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run nested soldr cargo build with inherited managed ZCCACHE_CACHE_DIR");

    assert!(
        output.status.success(),
        "inherited soldr-managed ZCCACHE_CACHE_DIR should not block nested soldr\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        path_display_variants(&child_zccache_dir)
            .iter()
            .any(|path| log.contains(&format!("zccache_dir={path}"))
                && log.contains(&format!("cache_dir={path}"))),
        "nested soldr should replace the inherited managed zccache dir with its own cache root: {log}"
    );
    assert!(
        !path_display_variants(&parent_zccache_dir)
            .iter()
            .any(|path| log.contains(&format!("cache_dir={path}"))),
        "nested soldr should not reuse the parent managed zccache dir: {log}"
    );
}

#[test]
fn managed_zccache_injects_normalized_path_remap_by_default() {
    let cache_root = unique_temp_dir("cargo-normalized-remap");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let repo_root = unique_temp_dir("cargo-normalized-remap-repo");
    let nested = repo_root.join("crates").join("demo");
    fs::create_dir_all(repo_root.join(".git")).expect("failed to create fake git root");
    fs::create_dir_all(&nested).expect("failed to create nested cwd");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .current_dir(&nested)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("ZCCACHE_PATH_REMAP")
        .env_remove("ZCCACHE_WORKTREE_ROOT")
        .env_remove("SOLDR_PATH_REMAP")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with normalized remap defaults");

    assert!(
        output.status.success(),
        "normalized remap front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("path_remap=auto"),
        "managed zccache should enable path remap by default: {log}"
    );
    assert!(
        path_display_variants(&repo_root)
            .iter()
            .any(|path| log.contains(&format!("worktree_root={path}"))),
        "managed zccache should pass the git root as ZCCACHE_WORKTREE_ROOT: {log}"
    );
}

#[test]
fn cargo_front_door_uses_custom_rustc_wrapper_from_env_var() {
    let cache_root = unique_temp_dir("cargo-custom-wrapper");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let wrapper = install_fake_wrapper(&log_path, "sccache");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_RUSTC_WRAPPER", &wrapper)
        .output()
        .expect("failed to run soldr cargo build with custom rustc wrapper");

    assert!(
        output.status.success(),
        "custom-wrapper front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains(&format!("cargo wrapper={}", wrapper.display())),
        "cargo should receive the custom wrapper path: {log}"
    );
    assert!(
        log.contains("sccache wrapper"),
        "custom wrapper should be invoked for rustc: {log}"
    );
    let expected_sccache_dir = cache_root.join("cache").join("sccache");
    assert!(
        path_display_variants(&expected_sccache_dir)
            .iter()
            .any(|path| log.contains(&format!("sccache_dir={path}"))),
        "cargo should receive soldr-owned SCCACHE_DIR at {}: {log}",
        expected_sccache_dir.display()
    );
    assert!(
        expected_sccache_dir.is_dir(),
        "soldr should pre-create the owned sccache cache dir at {}",
        expected_sccache_dir.display()
    );
    assert!(
        !log.contains(env!("CARGO_BIN_EXE_soldr")),
        "soldr should not stay in the wrapper slot when overridden: {log}"
    );
    assert!(
        !log.contains("zccache start")
            && !log.contains("zccache session-start")
            && !log.contains("zccache wrapper")
            && !log.contains("zccache session-end"),
        "managed zccache should be skipped when using a custom wrapper: {log}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("soldr: zccache session summary"),
        "custom wrapper path should not emit zccache session output: {stderr}"
    );
}

#[test]
fn custom_sccache_wrapper_preserves_caller_sccache_dir() {
    let cache_root = unique_temp_dir("cargo-custom-wrapper-preserve-sccache-dir");
    let caller_sccache_dir = unique_temp_dir("caller-sccache-dir");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let wrapper = install_fake_wrapper(&log_path, "sccache");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_RUSTC_WRAPPER", &wrapper)
        .env("SCCACHE_DIR", &caller_sccache_dir)
        .output()
        .expect("failed to run soldr cargo build with caller SCCACHE_DIR");

    assert!(
        output.status.success(),
        "custom-wrapper front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        path_display_variants(&caller_sccache_dir)
            .iter()
            .any(|path| log.contains(&format!("sccache_dir={path}"))),
        "cargo should preserve caller-provided SCCACHE_DIR at {}: {log}",
        caller_sccache_dir.display()
    );
    let soldr_sccache_dir = cache_root.join("cache").join("sccache");
    assert!(
        !path_display_variants(&soldr_sccache_dir)
            .iter()
            .any(|path| log.contains(&format!("sccache_dir={path}"))),
        "cargo should not override caller SCCACHE_DIR with {}: {log}",
        soldr_sccache_dir.display()
    );
}

#[test]
fn empty_rustc_wrapper_override_disables_wrapper_injection() {
    let cache_root = unique_temp_dir("cargo-wrapper-disabled");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_RUSTC_WRAPPER", "")
        .output()
        .expect("failed to run soldr cargo build with wrapper disabled");

    assert!(
        output.status.success(),
        "wrapper-disabled front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo wrapper= rustc="),
        "cargo should not receive a wrapper when override is empty: {log}"
    );
    assert!(
        !log.contains("zccache start")
            && !log.contains("zccache session-start")
            && !log.contains("zccache wrapper")
            && !log.contains("zccache session-end"),
        "managed zccache should be skipped when wrapper injection is disabled: {log}"
    );
    assert!(
        log.contains("rustc ") && log.contains("--crate-name demo"),
        "rustc should still run directly when wrapper injection is disabled: {log}"
    );
}

#[test]
fn no_cache_bypasses_wrapper_and_zccache() {
    let cache_root = unique_temp_dir("cargo-no-cache-fake");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["--no-cache", "cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr --no-cache cargo build with fake tools");

    assert!(
        output.status.success(),
        "no-cache front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cache=0"),
        "no-cache front door should propagate cache disable flag: {log}"
    );
    assert!(
        !log.contains("zccache start"),
        "no-cache front door should not start zccache: {log}"
    );
    assert!(
        !log.contains(env!("CARGO_BIN_EXE_soldr")),
        "no-cache front door should not set soldr as wrapper: {log}"
    );
    assert!(
        log.contains("rustc ") && log.contains("--crate-name demo"),
        "no-cache front door should call rustc directly: {log}"
    );
}

#[test]
fn rustc_wrapper_mode_passes_through_to_rustc() {
    let rustc = rustup_which("rustc");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg(rustc)
        .arg("--version")
        .output()
        .expect("failed to run soldr in wrapper mode");

    assert!(output.status.success(), "wrapper mode failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("rustc"),
        "unexpected rustc output: {stdout}"
    );
}

#[test]
fn repo_local_toolchain_homes_are_used_when_env_vars_are_unset() {
    let cache_root = unique_temp_dir("repo-local-toolchain-homes");
    let log_path = cache_root.join("tool.log");
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let repo_root = unique_temp_dir("repo-local-toolchain-root");
    let repo_cargo_home = repo_root.join(".cargo");
    let repo_rustup_home = repo_root.join(".rustup");
    let nested = repo_root.join("workspace").join("crate");
    fs::create_dir_all(&repo_cargo_home).expect("failed to create repo-local .cargo");
    fs::create_dir_all(&repo_rustup_home).expect("failed to create repo-local .rustup");
    fs::create_dir_all(&nested).expect("failed to create nested working dir");

    for args in [
        vec!["--no-cache", "cargo", "--version"],
        vec!["rustfmt", "--version"],
        vec!["--no-cache", "rustc", "--version"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
            .args(&args)
            .current_dir(&nested)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("PATH", isolated_test_path())
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap_or_else(|_| panic!("failed to run soldr with args {args:?}"));

        assert!(
            output.status.success(),
            "soldr invocation failed for {:?}\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let log = fs::read_to_string(&log_path).expect("failed to read fake rustup log");
    assert!(
        log_contains_toolchain_homes(
            &log,
            "rustup which cargo",
            &repo_cargo_home,
            &repo_rustup_home
        ),
        "cargo resolution should use repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(&log, "cargo", &repo_cargo_home, &repo_rustup_home),
        "cargo execution should inherit repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(
            &log,
            "rustup which rustfmt",
            &repo_cargo_home,
            &repo_rustup_home
        ),
        "rustfmt resolution should use repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(&log, "rustfmt", &repo_cargo_home, &repo_rustup_home),
        "rustfmt execution should inherit repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(
            &log,
            "rustup which rustc",
            &repo_cargo_home,
            &repo_rustup_home
        ),
        "rustc resolution should use repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(&log, "rustc", &repo_cargo_home, &repo_rustup_home),
        "rustc execution should inherit repo-local homes: {log}"
    );
}

#[test]
fn repo_local_cargo_bin_tools_work_without_rustup() {
    let cache_root = unique_temp_dir("repo-local-cargo-bin");
    let log_path = cache_root.join("tool.log");
    let rustup = install_failing_fake_rustup(&log_path);
    let repo_root = unique_temp_dir("repo-local-cargo-bin-root");
    let repo_cargo_bin = repo_root.join(".cargo").join("bin");
    let repo_rustup_home = repo_root.join(".rustup");
    let nested = repo_root.join("workspace").join("crate");
    fs::create_dir_all(&repo_cargo_bin).expect("failed to create repo-local .cargo/bin");
    // Anchor the rustup-home ancestor walk inside the test sandbox so it can't
    // climb up to a runner-installed `~/.rustup` (Windows GitHub runners put
    // TEMP under USERPROFILE, where `.rustup` typically exists).
    fs::create_dir_all(&repo_rustup_home).expect("failed to create repo-local .rustup");
    fs::create_dir_all(&nested).expect("failed to create nested working dir");
    install_fake_version_toolchain(&repo_cargo_bin, &log_path);

    for args in [
        vec!["--no-cache", "cargo", "--version"],
        vec!["rustfmt", "--version"],
        vec!["--no-cache", "rustc", "--version"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
            .args(&args)
            .current_dir(&nested)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("PATH", isolated_test_path())
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap_or_else(|_| panic!("failed to run soldr with args {args:?}"));

        assert!(
            output.status.success(),
            "soldr invocation failed for {:?}\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.lines().any(|line| line.starts_with("cargo ")),
        "expected repo-local cargo shim to run: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("rustfmt ")),
        "expected repo-local rustfmt shim to run: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("rustc ")),
        "expected repo-local rustc shim to run: {log}"
    );
    assert!(
        !log.lines().any(|line| line.starts_with("rustup ")),
        "repo-local .cargo/bin tools should bypass rustup entirely: {log}"
    );
}

#[test]
fn explicit_toolchain_home_env_vars_win_over_repo_local_homes() {
    let cache_root = unique_temp_dir("explicit-toolchain-homes");
    let log_path = cache_root.join("tool.log");
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let repo_root = unique_temp_dir("explicit-toolchain-repo");
    let repo_cargo_home = repo_root.join(".cargo");
    let repo_rustup_home = repo_root.join(".rustup");
    let nested = repo_root.join("workspace").join("crate");
    let explicit_cargo_home = unique_temp_dir("explicit-cargo-home");
    let explicit_rustup_home = unique_temp_dir("explicit-rustup-home");
    fs::create_dir_all(&repo_cargo_home).expect("failed to create repo-local .cargo");
    fs::create_dir_all(&repo_rustup_home).expect("failed to create repo-local .rustup");
    fs::create_dir_all(&nested).expect("failed to create nested working dir");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["--no-cache", "cargo", "--version"])
        .current_dir(&nested)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("CARGO_HOME", &explicit_cargo_home)
        .env("RUSTUP_HOME", &explicit_rustup_home)
        .env("PATH", isolated_test_path())
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .expect("failed to run soldr cargo --version with explicit homes");

    assert!(
        output.status.success(),
        "soldr cargo --version failed with explicit homes\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake rustup log");
    let explicit_cargo_home = explicit_cargo_home.display().to_string();
    let explicit_rustup_home = explicit_rustup_home.display().to_string();
    assert!(
        log.contains(&format!(
            "rustup which cargo cargo_home={explicit_cargo_home} rustup_home={explicit_rustup_home}"
        )),
        "cargo resolution should prefer explicit homes: {log}"
    );
    assert!(
        log.contains(&format!(
            "cargo cargo_home={explicit_cargo_home} rustup_home={explicit_rustup_home}"
        )),
        "cargo execution should inherit explicit homes: {log}"
    );
    assert!(
        !log.contains(&repo_cargo_home.display().to_string())
            && !log.contains(&repo_rustup_home.display().to_string()),
        "repo-local homes should not leak into explicit-home runs: {log}"
    );
}
