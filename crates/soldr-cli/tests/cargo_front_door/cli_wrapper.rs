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
fn rustup_resolution_failure_reports_raw_error_and_ci_guidance() {
    let output = Command::new(common::soldr_bin())
        .args(["--no-cache", "rustc", "--version"])
        .env("RUSTUP_TOOLCHAIN", "soldr-ci-missing-toolchain")
        .output()
        .expect("failed to run soldr --no-cache rustc --version with invalid RUSTUP_TOOLCHAIN");

    assert!(
        !output.status.success(),
        "expected soldr rustc --version to fail when rustup resolution fails"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to resolve rustc via rustup: error: override toolchain 'soldr-ci-missing-toolchain' is not installed"),
        "expected raw rustup stderr to be preserved: {stderr}"
    );
    assert!(
        stderr.contains(
            "the RUSTUP_TOOLCHAIN environment variable specifies an uninstalled toolchain"
        ),
        "expected raw rustup explanation in stderr: {stderr}"
    );
    assert!(
        stderr.contains("pins Rust in rust-toolchain.toml"),
        "expected rust-toolchain.toml guidance in stderr: {stderr}"
    );
    assert!(
        stderr.contains("generic stable toolchain"),
        "expected exact-channel guidance in stderr: {stderr}"
    );
    assert!(
        stderr.contains("RUSTUP_TOOLCHAIN"),
        "expected RUSTUP_TOOLCHAIN guidance in stderr: {stderr}"
    );
    assert!(
        stderr.contains("setup-soldr action path"),
        "expected setup-soldr guidance in stderr: {stderr}"
    );
}
#[test]
fn cargo_front_door_forces_msvc_target_even_with_polluted_path() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let fake_tools = unique_temp_dir("fake-tools");
    fs::write(
        fake_tools.join("cargo.cmd"),
        "@echo off\r\necho fake cargo should not be used 1>&2\r\nexit /b 1\r\n",
    )
    .expect("failed to write fake cargo.cmd");
    fs::write(
        fake_tools.join("rustc.cmd"),
        "@echo off\r\necho fake rustc should not be used 1>&2\r\nexit /b 1\r\n",
    )
    .expect("failed to write fake rustc.cmd");

    let target_dir = unique_temp_dir("target-dir");
    // soldr#1040 phase 2: resolve fixtures via SOLDR_TEST_FIXTURES_DIR
    // with CARGO_MANIFEST_DIR fallback so a runner that downloaded
    // fixtures alongside the test binary works without code changes.
    let fixture = common::fixtures_dir().join("windows-msvc-default");
    if !fixture.is_dir() {
        panic!(
            "windows-msvc-default fixture not found at {} — \
             SOLDR_TEST_FIXTURES_DIR may need to be set",
            fixture.display()
        );
    }
    let output = Command::new(common::soldr_bin())
        .args(["--no-cache", "cargo", "build"])
        .current_dir(&fixture)
        .env("PATH", prepend_to_path(&fake_tools))
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("SOLDR_CACHE_DIR", unique_temp_dir("msvc-cache-root"))
        .output()
        .expect("failed to run soldr cargo build");

    assert!(
        output.status.success(),
        "soldr cargo build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // #692: the MSVC target soldr forces is host-arch-dependent
    // (CLAUDE.md: "Default to `x86_64-pc-windows-msvc` (or aarch64)").
    // On a Windows ARM64 runner soldr correctly picks
    // `aarch64-pc-windows-msvc`, so we have to look in the matching
    // sub-directory under target/. Hardcoding `x86_64-pc-windows-msvc`
    // broke ci.yml on the Windows ARM64 job.
    let host_msvc_triple = if matches!(
        soldr_platform::host::facts::arch(),
        soldr_platform::host::facts::HostArch::Aarch64
    ) {
        "aarch64-pc-windows-msvc"
    } else {
        "x86_64-pc-windows-msvc"
    };
    let artifact = target_dir
        .join(host_msvc_triple)
        .join("debug")
        .join("windows-msvc-default.exe");
    assert!(
        artifact.exists(),
        "expected MSVC target artifact at {}",
        artifact.display()
    );
}

/// Regression test for issue #324: RUSTC_WRAPPER mode must propagate the
/// rustc exit code even when the source is read from stdin ("-").
///
/// Before the fix, zccache consumed stdin to hash the source. Rustc then
/// received an empty stdin, compiled nothing, and exited 0 — masking E0554
/// and similar errors that build-script feature probes depend on.
///
/// The test exercises the direct-to-rustc path (no zccache configured) to
/// keep it self-contained and fast. The spill-to-tempfile logic runs
/// regardless of whether zccache is in the chain.
#[test]
fn wrapper_mode_stdin_source_propagates_nonzero_exit_code() {
    let rustc = rustup_which("rustc");
    let out_dir = unique_temp_dir("wrapper-stdin-exit");

    // A source that is valid Rust but uses an unstable feature gate.
    // On stable rustc this must fail with E0554 (exit != 0).
    let probe_source = b"#![allow(stable_features)]\n#![feature(rustc_attrs)]\n";

    // Invoke soldr as RUSTC_WRAPPER: soldr <rustc-path> - <flags...>
    // Disable the cache to avoid a zccache binary being required.
    let mut child = Command::new(common::soldr_bin())
        .args([
            rustc.as_str(),
            "-",
            "--crate-type=lib",
            "--emit=metadata",
            "--out-dir",
            out_dir.to_str().expect("non-UTF-8 temp dir"),
        ])
        .env("SOLDR_CACHE_ENABLED", "0")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn soldr in wrapper mode");

    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(probe_source)
        .expect("failed to write probe source to soldr stdin");

    let output = child
        .wait_with_output()
        .expect("failed to wait for soldr wrapper");

    assert!(
        !output.status.success(),
        "soldr wrapper mode with stdin source must propagate non-zero rustc exit code\n\
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
