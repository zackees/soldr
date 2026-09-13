//! soldr#3195: no test may download a Rust toolchain.
//!
//! The Nextest wrapper arms `SOLDR_TEST_FORBID_TOOLCHAIN_INSTALL` for every test
//! process. This pins the behaviour that makes the guard worth arming: a soldr
//! path that would install a toolchain through the real rustup fails with the
//! tripwire's diagnostic instead of downloading. Kept apart from
//! `cli_cargo_basic` so that file stays under the per-file line ceiling.

use crate::common::*;
use std::fs;

/// With the guard armed and no fake rustup, the `+toolchain` install must fail
/// with the guard's diagnostic instead of invoking the real rustup, which
/// downloads a toolchain the host lacks.
#[test]
fn plus_toolchain_install_trips_the_test_download_guard() {
    let cache_root = unique_temp_dir("cargo-plus-toolchain-guard");
    let tool_dir = unique_temp_dir("cargo-plus-toolchain-guard-bin");
    let log_path = cache_root.join("cargo.log");
    let cargo = fake_script_path(&tool_dir, "cargo");
    let rustc = fake_script_path(&tool_dir, "rustc");
    // The install happens before cargo is ever executed, so a cargo that does
    // nothing is enough; the rustc fake answers soldr's version probes.
    write_fake_script(
        &cargo,
        if matches!(
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::HostOs::Windows
        ) {
            "@echo off\nexit /b 0\n"
        } else {
            "#!/bin/sh\nexit 0\n"
        },
    );
    write_fake_script(&rustc, &fake_rustc_script(&log_path));

    let output = isolated_soldr_command()
        .args([
            "--no-cache",
            "cargo",
            // A channel no host has, so the front door always reaches its install
            // path instead of finding the toolchain already present.
            "+nightly-2001-01-01",
            "test",
            "--manifest-path",
            "dylints/ban_manual_slash_normalize/Cargo.toml",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_FORBID_TOOLCHAIN_INSTALL", "1")
        .env_remove("SOLDR_TEST_RUSTUP_BIN")
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .expect("failed to run soldr cargo +toolchain under the download guard");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SOLDR_TEST_FORBID_TOOLCHAIN_INSTALL"),
        "the real rustup must not be asked to install a toolchain under test\nstatus: {:?}\nstderr:\n{stderr}",
        output.status
    );
    let _ = fs::remove_dir_all(&cache_root);
    let _ = fs::remove_dir_all(&tool_dir);
}
