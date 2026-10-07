//! Hermetic Clippy workspace-wrapper routing fixture.

use crate::common::*;
use std::fs;

#[test]
fn cargo_clippy_routes_workspace_clippy_driver_through_zccache() {
    let cache_root = unique_temp_dir("cargo-clippy-clippy-driver-zccache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache, _clippy_driver) = install_fake_clippy_toolchain(&log_path);
    // This fixture supplies every compiler tool. A real rustup component
    // probe/install is unrelated to the wrapper route it exercises.
    let tool_dir = cargo.parent().expect("fake toolchain directory");
    let rustup = fake_script_path(tool_dir, "rustup");
    write_fake_script(
        &rustup,
        &if matches!(
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::HostOs::Windows
        ) {
            format!(
                "@echo off\necho unexpected rustup %*>>\"{}\"\nexit /b 1\n",
                log_path.display()
            )
        } else {
            format!(
                "#!/bin/sh\necho \"unexpected rustup $*\" >> \"{}\"\nexit 1\n",
                log_path.display()
            )
        },
    );
    let output = isolated_soldr_command_in(&cache_root)
        .args(["cargo", "clippy"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("PATH", prepend_to_path(tool_dir))
        .env("SOLDR_NO_AUTO_COMPONENT", "1")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo clippy with fake tools");
    assert!(
        output.status.success(),
        "cargo clippy front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        !log.contains("unexpected rustup which cargo-clippy")
            && !log.contains("unexpected rustup component add"),
        "the fake Clippy routing fixture must not probe/install real components: {log}"
    );
    assert!(
        log.contains("cargo wrapper=") && log.contains("workspace_wrapper="),
        "cargo clippy should retain Soldr-owned compiler shim routing: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("clippy-driver ")),
        "the workspace Clippy driver must actually execute: {log}"
    );
}
