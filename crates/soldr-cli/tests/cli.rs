use std::process::Command;
#[cfg(windows)]
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn rustup_which(tool: &str) -> String {
    let output = Command::new("rustup")
        .args(["which", tool])
        .output()
        .expect("failed to resolve tool with rustup");
    assert!(output.status.success(), "rustup which failed for {tool}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn version_command_prints_workspace_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("version")
        .output()
        .expect("failed to run soldr version");

    assert!(output.status.success(), "version command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        format!("soldr {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn help_lists_phase_one_command_surface() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("--help")
        .output()
        .expect("failed to run soldr --help");

    assert!(output.status.success(), "help command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("status"), "help output missing status");
    assert!(stdout.contains("clean"), "help output missing clean");
    assert!(stdout.contains("config"), "help output missing config");
    assert!(stdout.contains("cache"), "help output missing cache");
    assert!(stdout.contains("version"), "help output missing version");
    assert!(stdout.contains("cargo"), "help output missing cargo");
}

#[test]
fn cargo_front_door_runs_real_cargo() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "--version"])
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
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "--no-cache", "--version"])
        .output()
        .expect("failed to run soldr cargo --no-cache --version");

    assert!(
        output.status.success(),
        "cargo front door with --no-cache failed\nstdout:\n{}\nstderr:\n{}",
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
fn status_reports_cache_control_defaults() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("status")
        .output()
        .expect("failed to run soldr status");

    assert!(output.status.success(), "status command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cache dir:"),
        "status missing cache dir: {stdout}"
    );
    assert!(
        stdout.contains("cache default: enabled"),
        "status missing cache default: {stdout}"
    );
}

#[cfg(windows)]
fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

#[cfg(windows)]
fn prepend_to_path(dir: &Path) -> std::ffi::OsString {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(&existing));
    std::env::join_paths(paths).expect("failed to join PATH")
}

#[cfg(windows)]
#[test]
fn cargo_front_door_forces_msvc_target_even_with_polluted_path() {
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
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("windows-msvc-default");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .current_dir(&fixture)
        .env("PATH", prepend_to_path(&fake_tools))
        .env("CARGO_TARGET_DIR", &target_dir)
        .output()
        .expect("failed to run soldr cargo build");

    assert!(
        output.status.success(),
        "soldr cargo build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let artifact = target_dir
        .join("x86_64-pc-windows-msvc")
        .join("debug")
        .join("windows-msvc-default.exe");
    assert!(
        artifact.exists(),
        "expected MSVC target artifact at {}",
        artifact.display()
    );
}
