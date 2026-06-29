//! Integration tests for the workspace-level trampoline shipped in
//! issue #354 (Tier L3 of #352). Mirrors the matrix in
//! `cli_cargo_run_trampoline.rs` but for `build`, `check`, and `clippy`.
//!
//! Each test spins up a tiny cargo project, runs `soldr cargo <verb>`
//! cold to seed the sidecar, then runs again with a broken-cargo stub on
//! `SOLDR_TEST_CARGO_BIN` to prove that the warm path never spawns cargo.

#![allow(unused_imports)]

mod common;

use common::*;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn soldr_bin() -> std::path::PathBuf {
    // soldr#1039 phase 1.
    common::soldr_bin()
}

/// Tiny crate with a binary and a small library so `cargo build` emits
/// multiple artifacts (the binary itself, the librlib, and a deps rmeta).
fn make_project(label: &str) -> PathBuf {
    let dir = unique_temp_dir(label);
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "workspace_trampoline_demo"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "workspace_trampoline_demo"
path = "src/main.rs"

[lib]
name = "workspace_trampoline_demo"
path = "src/lib.rs"
"#,
    )
    .expect("write Cargo.toml");
    fs::write(
        src.join("main.rs"),
        r#"fn main() {
    println!("hello {}", workspace_trampoline_demo::name());
}
"#,
    )
    .expect("write main.rs");
    fs::write(
        src.join("lib.rs"),
        r#"pub fn name() -> &'static str {
    "workspace"
}
"#,
    )
    .expect("write lib.rs");
    dir
}

fn broken_cargo_stub(dir: &Path) -> PathBuf {
    let path = fake_script_path(dir, "broken-cargo");
    #[cfg(windows)]
    let body = "@echo off\necho broken cargo should not have been spawned 1>&2\nexit /b 99\n";
    #[cfg(not(windows))]
    let body = "#!/bin/sh\necho 'broken cargo should not have been spawned' >&2\nexit 99\n";
    write_fake_script(&path, body);
    path
}

fn run_soldr<I, S>(project: &Path, env_overrides: &[(&str, &str)], args: I) -> std::process::Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(soldr_bin());
    cmd.current_dir(project);
    cmd.args(args);
    for (k, v) in env_overrides {
        cmd.env(k, v);
    }
    cmd.env("SOLDR_CACHE_DIR", project.join(".soldr-cache"));
    // Tests that point SOLDR_TEST_CARGO_BIN at a broken stub specifically
    // want the workspace trampoline to fire (and skip cargo); opt back in
    // since the front door otherwise suppresses the workspace trampoline
    // whenever SOLDR_TEST_CARGO_BIN is set.
    cmd.env("SOLDR_TEST_FORCE_WORKSPACE_TRAMPOLINE", "1");
    cmd.env_remove("SOLDR_TARGET_CACHE_MODE");
    cmd.env_remove("SOLDR_BUILD_CACHE_MODE");
    cmd.output().expect("spawn soldr")
}

fn run_verb_cold(project: &Path, verb: &str, extra: &[&str]) -> std::process::Output {
    let mut argv: Vec<&str> = vec!["--no-cache", "cargo", verb];
    argv.extend_from_slice(extra);
    run_soldr(project, &[], argv)
}

fn profile_root(project: &Path, profile: &str) -> PathBuf {
    let mut root = project.join("target");
    if cfg!(windows) {
        let target_root = project.join("target");
        if let Ok(entries) = fs::read_dir(&target_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let probe = path.join(profile);
                    if probe.is_dir() {
                        return probe;
                    }
                }
            }
        }
    }
    root.push(profile);
    root
}

fn workspace_sidecar(project: &Path, verb: &str, profile: &str) -> PathBuf {
    profile_root(project, profile)
        .join(".soldr-trampoline")
        .join(format!("workspace-{verb}.toml"))
}

// ---------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------

#[test]
fn build_cold_writes_workspace_sidecar() {
    let project = make_project("ws-build-cold");
    let out = run_verb_cold(&project, "build", &[]);
    assert!(
        out.status.success(),
        "cold build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let sidecar = workspace_sidecar(&project, "build", "debug");
    assert!(
        sidecar.is_file(),
        "sidecar not written: {}",
        sidecar.display()
    );
    let text = fs::read_to_string(&sidecar).expect("read sidecar");
    assert!(text.contains("schema_version"));
    assert!(text.contains("verb = \"build\""));
    assert!(text.contains("cargo_args_fingerprint"));
    assert!(text.contains("outputs"));
    assert!(text.contains("source_files"));
}

#[test]
fn build_warm_skips_cargo_entirely() {
    let project = make_project("ws-build-warm");
    let cold = run_verb_cold(&project, "build", &[]);
    assert!(
        cold.status.success(),
        "seed cold build failed: {}",
        String::from_utf8_lossy(&cold.stderr)
    );

    let stub_dir = unique_temp_dir("ws-build-warm-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let warm = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "build"],
    );
    assert!(
        warm.status.success(),
        "warm build should hit trampoline (cargo was spawned?)\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&warm.stdout),
        String::from_utf8_lossy(&warm.stderr)
    );
    let stderr = String::from_utf8_lossy(&warm.stderr);
    assert!(
        !stderr.contains("broken cargo should not have been spawned"),
        "trampoline fell through and broken cargo was invoked: {stderr}"
    );
}

#[test]
fn build_edit_source_forces_fall_through() {
    let project = make_project("ws-build-edit");
    let cold = run_verb_cold(&project, "build", &[]);
    assert!(cold.status.success(), "seed build failed");

    let main_rs = project.join("src").join("main.rs");
    let mut text = fs::read_to_string(&main_rs).expect("read main.rs");
    text.push_str("// edit\n");
    std::thread::sleep(Duration::from_millis(1100));
    fs::write(&main_rs, text).expect("write main.rs");

    let stub_dir = unique_temp_dir("ws-build-edit-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "build"],
    );
    assert!(
        !out.status.success(),
        "edit-source should have forced fall-through to (broken) cargo"
    );
}

#[test]
fn build_missing_output_forces_fall_through() {
    let project = make_project("ws-build-no-output");
    let cold = run_verb_cold(&project, "build", &[]);
    assert!(cold.status.success(), "seed build failed");

    // Read the recorded outputs from the sidecar and delete the first
    // existing one to simulate a `cargo clean` having run.
    let sidecar = workspace_sidecar(&project, "build", "debug");
    let text = fs::read_to_string(&sidecar).expect("read sidecar");
    let value: toml::Value = text.parse().expect("parse sidecar toml");
    let outputs = value
        .get("outputs")
        .and_then(|v| v.as_array())
        .expect("outputs array");
    let mut removed = false;
    for entry in outputs {
        let Some(path) = entry.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let p = PathBuf::from(path);
        if p.exists() {
            fs::remove_file(&p).expect("remove output");
            removed = true;
            break;
        }
    }
    assert!(removed, "no recorded output found on disk to delete");

    let stub_dir = unique_temp_dir("ws-build-no-output-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "build"],
    );
    assert!(
        !out.status.success(),
        "missing output should have forced fall-through"
    );
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

#[test]
fn check_cold_writes_sidecar_with_rmeta_refs() {
    let project = make_project("ws-check-cold");
    let out = run_verb_cold(&project, "check", &[]);
    assert!(
        out.status.success(),
        "cold check failed\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sidecar = workspace_sidecar(&project, "check", "debug");
    assert!(sidecar.is_file(), "sidecar missing: {}", sidecar.display());
    let text = fs::read_to_string(&sidecar).expect("read sidecar");
    assert!(text.contains("verb = \"check\""));
    // At least one recorded output should be a .rmeta or rlib.
    assert!(
        text.contains(".rmeta") || text.contains(".rlib"),
        "expected sidecar to reference at least one .rmeta/.rlib: {text}"
    );
}

#[test]
fn check_warm_skips_cargo() {
    let project = make_project("ws-check-warm");
    let cold = run_verb_cold(&project, "check", &[]);
    assert!(cold.status.success(), "seed check failed");

    let stub_dir = unique_temp_dir("ws-check-warm-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let warm = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "check"],
    );
    assert!(
        warm.status.success(),
        "warm check should hit trampoline\nstderr:\n{}",
        String::from_utf8_lossy(&warm.stderr)
    );
}

#[test]
fn check_deleting_rmeta_falls_through() {
    let project = make_project("ws-check-no-rmeta");
    let cold = run_verb_cold(&project, "check", &[]);
    assert!(cold.status.success(), "seed check failed");

    // Find one of the recorded outputs that's a .rmeta and remove it.
    let sidecar = workspace_sidecar(&project, "check", "debug");
    let text = fs::read_to_string(&sidecar).expect("read sidecar");
    let value: toml::Value = text.parse().expect("parse sidecar toml");
    let outputs = value
        .get("outputs")
        .and_then(|v| v.as_array())
        .expect("outputs array");
    let mut removed = false;
    for entry in outputs {
        let Some(path) = entry.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        if path.ends_with(".rmeta") {
            let p = PathBuf::from(path);
            if p.exists() {
                fs::remove_file(&p).expect("remove rmeta");
                removed = true;
                break;
            }
        }
    }
    assert!(removed, "no .rmeta recorded in sidecar to delete");

    let stub_dir = unique_temp_dir("ws-check-no-rmeta-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "check"],
    );
    assert!(
        !out.status.success(),
        "deleting a .rmeta should force fall-through"
    );
}

// ---------------------------------------------------------------------------
// clippy
// ---------------------------------------------------------------------------

fn clippy_available() -> bool {
    Command::new("cargo")
        .args(["clippy", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn clippy_cold_captures_output() {
    if !clippy_available() {
        eprintln!("skipping: cargo clippy not available");
        return;
    }
    let project = make_project("ws-clippy-cold");
    let out = run_verb_cold(&project, "clippy", &[]);
    assert!(
        out.status.success(),
        "cold clippy failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let sidecar = workspace_sidecar(&project, "clippy", "debug");
    assert!(sidecar.is_file(), "sidecar missing: {}", sidecar.display());
    let text = fs::read_to_string(&sidecar).expect("read sidecar");
    assert!(text.contains("verb = \"clippy\""));
    assert!(
        text.contains("[clippy_capture]") || text.contains("clippy_capture"),
        "sidecar should contain clippy_capture block: {text}"
    );

    let stdout_gz = profile_root(&project, "debug")
        .join(".soldr-trampoline")
        .join("workspace-clippy.stdout.gz");
    let stderr_gz = profile_root(&project, "debug")
        .join(".soldr-trampoline")
        .join("workspace-clippy.stderr.gz");
    assert!(stdout_gz.is_file(), "stdout.gz missing");
    assert!(stderr_gz.is_file(), "stderr.gz missing");
}

#[test]
fn clippy_warm_replays_diagnostics_and_skips_cargo() {
    if !clippy_available() {
        eprintln!("skipping: cargo clippy not available");
        return;
    }
    let project = make_project("ws-clippy-warm");
    let cold = run_verb_cold(&project, "clippy", &[]);
    assert!(
        cold.status.success(),
        "seed clippy failed: {}",
        String::from_utf8_lossy(&cold.stderr)
    );

    let stub_dir = unique_temp_dir("ws-clippy-warm-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let warm = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "clippy"],
    );
    assert!(
        warm.status.success(),
        "warm clippy should hit trampoline\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&warm.stdout),
        String::from_utf8_lossy(&warm.stderr)
    );
    let warm_stderr = String::from_utf8_lossy(&warm.stderr);
    assert!(
        !warm_stderr.contains("broken cargo should not have been spawned"),
        "broken cargo was invoked on warm clippy path: {warm_stderr}"
    );
}

#[test]
fn clippy_edit_source_forces_fall_through() {
    if !clippy_available() {
        eprintln!("skipping: cargo clippy not available");
        return;
    }
    let project = make_project("ws-clippy-edit");
    let cold = run_verb_cold(&project, "clippy", &[]);
    assert!(cold.status.success(), "seed clippy failed");

    let main_rs = project.join("src").join("main.rs");
    let mut text = fs::read_to_string(&main_rs).expect("read main.rs");
    text.push_str("// edit\n");
    std::thread::sleep(Duration::from_millis(1100));
    fs::write(&main_rs, text).expect("write main.rs");

    let stub_dir = unique_temp_dir("ws-clippy-edit-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "clippy"],
    );
    assert!(
        !out.status.success(),
        "clippy edit-source should have forced fall-through"
    );
}

// ---------------------------------------------------------------------------
// Cross-verb sanity: build sidecar does not leak into check warm path
// ---------------------------------------------------------------------------

#[test]
fn build_and_check_have_independent_sidecars() {
    let project = make_project("ws-cross-verbs");
    let b = run_verb_cold(&project, "build", &[]);
    assert!(b.status.success(), "cold build failed");
    let c = run_verb_cold(&project, "check", &[]);
    assert!(c.status.success(), "cold check failed");
    assert!(workspace_sidecar(&project, "build", "debug").is_file());
    assert!(workspace_sidecar(&project, "check", "debug").is_file());
}

#[test]
fn no_trampoline_flag_forces_fall_through_for_build() {
    let project = make_project("ws-build-opt-out");
    let cold = run_verb_cold(&project, "build", &[]);
    assert!(cold.status.success(), "seed build failed");

    let stub_dir = unique_temp_dir("ws-build-opt-out-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "build", "--no-trampoline"],
    );
    assert!(
        !out.status.success(),
        "--no-trampoline should force fall-through; broken cargo must be invoked"
    );
}

#[test]
fn env_var_opt_out_forces_fall_through_for_check() {
    let project = make_project("ws-check-env-opt-out");
    let cold = run_verb_cold(&project, "check", &[]);
    assert!(cold.status.success(), "seed check failed");

    let stub_dir = unique_temp_dir("ws-check-env-opt-out-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[
            ("SOLDR_TEST_CARGO_BIN", &broken_str),
            ("SOLDR_NO_TRAMPOLINE", "1"),
        ],
        ["--no-cache", "cargo", "check"],
    );
    assert!(
        !out.status.success(),
        "SOLDR_NO_TRAMPOLINE=1 should force fall-through"
    );
}
