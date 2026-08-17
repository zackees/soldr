//! soldr#2546 slice 2 — GREEN fixture: `soldr --debug cargo build` records
//! the direct cargo child AND at least one live descendant through the
//! running-process observer (`with_observer_and_command`,
//! running-process#1023).
//!
//! The fake cargo spawns a short-lived grandchild (`sleep` / `ping`), so the
//! observed tree is soldr -> fake cargo -> grandchild — the depth the
//! per-OS LaunchedProcessTree backends exist to notice (subreaper+/proc on
//! Linux, Job Object IOCP on Windows, kqueue EVFILT_PROC on macOS).

mod common;

use common::*;
use std::fs;
use std::path::{Path, PathBuf};

/// A fake cargo whose only job is to hold a grandchild alive long enough
/// for every descendant backend's polling window (Linux scans at 50ms).
fn write_fixture_workspace(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("fixture src dir");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("fixture manifest");
    fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("fixture main");
}

fn install_grandchild_spawning_cargo(log_path: &Path) -> PathBuf {
    let dir = unique_temp_dir("debug-trace-observer");
    let cargo = fake_script_path(&dir, "cargo");
    let script = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo cargo %*>>\"{0}\"\n\
             ping -n 2 127.0.0.1 >nul\n\
             echo fake-cargo-done\n\
             exit /b 0\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"cargo $@\" >> \"{0}\"\n\
             sleep 1\n\
             echo fake-cargo-done\n\
             exit 0\n",
            log_path.display()
        )
    };
    write_fake_script(&cargo, &script);
    cargo
}

#[test]
fn debug_build_records_the_direct_child_and_a_descendant() {
    let cache_root = unique_temp_dir("debug-trace-observed-build");
    let log_path = cache_root.join("tool.log");
    write_fixture_workspace(&cache_root);
    let cargo = install_grandchild_spawning_cargo(&log_path);

    let output = isolated_soldr_command()
        .args(["--debug", "cargo", "build"])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("ZCCACHE_DISABLE", "1")
        .output()
        .expect("failed to run soldr --debug cargo build with the fake cargo");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "--debug build failed\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );

    // The direct child went through the observed spawn path...
    assert!(
        stderr.contains("(cargo, observed)"),
        "expected the observed spawn announcement: {stderr}"
    );
    assert!(
        stderr.contains("soldr debug: process timeline ->"),
        "expected the JSONL pointer: {stderr}"
    );
    // ...and the grandchild was noticed by the descendant backend.
    assert!(
        stderr.contains("descendant-started"),
        "expected a descendant-started event for the fake cargo's grandchild: {stderr}"
    );

    // The end-of-run summary identifies observed/incomplete descendants
    // (soldr#2546 acceptance: preserve ordering + identify unobserved exits).
    assert!(
        stderr.contains("summary (cargo): descendants started="),
        "expected the summary line: {stderr}"
    );

    // The fake cargo actually ran (exit-code and stdio passthrough intact).
    let log = fs::read_to_string(&log_path).expect("fake cargo log");
    assert!(
        log.contains("cargo build"),
        "fake cargo saw the verb: {log}"
    );
}

#[test]
fn without_debug_the_observed_path_is_not_used() {
    let cache_root = unique_temp_dir("debug-trace-plain-build");
    let log_path = cache_root.join("tool.log");
    write_fixture_workspace(&cache_root);
    let cargo = install_grandchild_spawning_cargo(&log_path);

    let output = isolated_soldr_command()
        .args(["cargo", "build"])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("ZCCACHE_DISABLE", "1")
        .output()
        .expect("failed to run plain soldr cargo build with the fake cargo");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "plain build failed: {stderr}");
    assert!(
        !stderr.contains("soldr debug:"),
        "no debug-trace output without the flag: {stderr}"
    );
}
