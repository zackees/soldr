//! Integration tests for the `soldr cargo run` trampoline (issue #344).
//!
//! These tests build a tiny cargo project in a tempdir, run `soldr cargo
//! run` against it, and assert that:
//!   - the cold invocation falls through to real cargo and writes a
//!     sidecar at `.soldr-trampoline/<bin>.toml`,
//!   - the warm invocation hits the trampoline fast path and exec's the
//!     binary directly (verified by pointing `SOLDR_TEST_CARGO_BIN` at a
//!     broken stub — if cargo gets spawned the build fails),
//!   - editing a source, bumping features, flipping `RUSTFLAGS`,
//!     switching profile, etc. all fall through correctly,
//!   - the opt-outs (`--no-trampoline`, `SOLDR_NO_TRAMPOLINE=1`) bypass
//!     the fast path even when the sidecar is fresh,
//!   - args passed via `--` reach the binary on both paths.

#![allow(unused_imports)]

mod common;

use common::*;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

fn soldr_bin() -> &'static str {
    env!("CARGO_BIN_EXE_soldr")
}

/// Build a tiny project containing a single binary that prints
/// `hello <args...>` so we can assert it ran and that argv passed
/// through correctly.
fn make_project(label: &str) -> PathBuf {
    let dir = unique_temp_dir(label);
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "trampoline_demo"
version = "0.1.0"
edition = "2021"

[features]
default = []
alpha = []
beta = []

[[bin]]
name = "trampoline_demo"
path = "src/main.rs"
"#,
    )
    .expect("write Cargo.toml");
    fs::write(
        src.join("main.rs"),
        r#"fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("hello {}", args.join(" "));
}
"#,
    )
    .expect("write main.rs");
    dir
}

/// Path to a "broken cargo" stub that fails fast. When tests set
/// `SOLDR_TEST_CARGO_BIN` to this, any cargo invocation through soldr
/// will fail — which is exactly how we prove the trampoline took the
/// fast path.
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
    // Trampoline should not pull in zccache or the cache daemon.
    cmd.env("SOLDR_CACHE_DIR", project.join(".soldr-cache"));
    cmd.env_remove("SOLDR_TARGET_CACHE_MODE");
    cmd.env_remove("SOLDR_BUILD_CACHE_MODE");
    cmd.output().expect("spawn soldr")
}

fn run_cold(project: &Path, extra_args: &[&str]) -> std::process::Output {
    let mut argv: Vec<&str> = vec!["--no-cache", "cargo", "run"];
    argv.extend_from_slice(extra_args);
    run_soldr(project, &[], argv)
}

/// Return the effective `target/<triple?>/<profile>/` directory that the
/// soldr cargo front door will land artifacts in. On Windows soldr
/// injects `CARGO_BUILD_TARGET=<host>` by default, so artifacts live
/// under `target/<host_triple>/<profile>/`. On Unix the host triple is
/// left implicit and artifacts live at `target/<profile>/`.
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

fn project_sidecar(project: &Path, bin: &str, profile: &str) -> PathBuf {
    profile_root(project, profile)
        .join(".soldr-trampoline")
        .join(format!("{bin}.toml"))
}

fn project_binary(project: &Path, bin: &str, profile: &str) -> PathBuf {
    let mut p = profile_root(project, profile).join(bin);
    if cfg!(windows) {
        p.set_extension("exe");
    }
    p
}

#[test]
fn cold_invocation_writes_sidecar_and_runs_binary() {
    let project = make_project("trampoline-cold");
    let out = run_cold(&project, &["--", "world"]);
    assert!(
        out.status.success(),
        "cold invocation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello world"),
        "expected binary stdout to contain 'hello world': {stdout}"
    );
    let sidecar = project_sidecar(&project, "trampoline_demo", "debug");
    assert!(
        sidecar.is_file(),
        "sidecar not written: {}",
        sidecar.display()
    );
    let text = fs::read_to_string(&sidecar).expect("read sidecar");
    assert!(text.contains("cargo_args_fingerprint"));
    assert!(text.contains("blake3:"));
    assert!(text.contains("source_files"));
}

#[test]
fn warm_invocation_skips_cargo_and_execs_binary() {
    let project = make_project("trampoline-warm");
    // Cold build via real cargo to seed the sidecar.
    let cold = run_cold(&project, &[]);
    assert!(
        cold.status.success(),
        "seed cold build failed:\n{}",
        String::from_utf8_lossy(&cold.stderr)
    );

    // Now make cargo unusable. If the trampoline works, soldr never
    // spawns cargo and the binary's stdout still appears.
    let stub_dir = unique_temp_dir("trampoline-warm-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run", "--", "ping"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "warm trampoline path failed (cargo was spawned?)\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("hello ping"),
        "binary stdout missing on warm path: {stdout}"
    );
    assert!(
        !stderr.contains("broken cargo should not have been spawned"),
        "trampoline fell through and broken cargo was invoked: {stderr}"
    );
}

#[test]
fn edit_source_forces_fall_through() {
    let project = make_project("trampoline-edit");
    let cold = run_cold(&project, &[]);
    assert!(cold.status.success(), "seed build failed");

    // Bump mtime/size of main.rs by appending a no-op comment.
    let main_rs = project.join("src").join("main.rs");
    let mut text = fs::read_to_string(&main_rs).expect("read main.rs");
    text.push_str("// edit\n");
    // Wait long enough that the new mtime is unambiguously newer than the
    // sidecar's recorded value (some filesystems have ~1s mtime resolution).
    std::thread::sleep(Duration::from_millis(1100));
    fs::write(&main_rs, text).expect("write main.rs");

    // With a broken cargo and an edited source, the trampoline must
    // fall through and the broken cargo must be invoked → non-zero exit.
    let stub_dir = unique_temp_dir("trampoline-edit-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run"],
    );
    assert!(
        !out.status.success(),
        "edit-source should have forced fall-through to (broken) cargo"
    );
}

#[test]
fn no_trampoline_flag_forces_fall_through_to_cargo() {
    let project = make_project("trampoline-flag-opt-out");
    let cold = run_cold(&project, &[]);
    assert!(cold.status.success(), "seed build failed");

    let stub_dir = unique_temp_dir("trampoline-flag-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run", "--no-trampoline"],
    );
    assert!(
        !out.status.success(),
        "--no-trampoline should have forced fall-through; broken cargo must be invoked"
    );
}

#[test]
fn env_var_opt_out_forces_fall_through() {
    let project = make_project("trampoline-env-opt-out");
    let cold = run_cold(&project, &[]);
    assert!(cold.status.success(), "seed build failed");

    let stub_dir = unique_temp_dir("trampoline-env-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[
            ("SOLDR_TEST_CARGO_BIN", &broken_str),
            ("SOLDR_NO_TRAMPOLINE", "1"),
        ],
        ["--no-cache", "cargo", "run"],
    );
    assert!(
        !out.status.success(),
        "SOLDR_NO_TRAMPOLINE=1 should have forced fall-through; broken cargo must be invoked"
    );
}

#[test]
fn features_change_forces_fall_through() {
    let project = make_project("trampoline-features");
    let cold = run_cold(&project, &[]);
    assert!(cold.status.success(), "seed build failed");

    let stub_dir = unique_temp_dir("trampoline-features-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run", "--features", "alpha"],
    );
    assert!(
        !out.status.success(),
        "feature flag change should force fall-through"
    );
}

#[test]
fn rustflags_change_forces_fall_through() {
    let project = make_project("trampoline-rustflags");
    let cold = run_cold(&project, &[]);
    assert!(cold.status.success(), "seed build failed");

    let stub_dir = unique_temp_dir("trampoline-rustflags-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[
            ("SOLDR_TEST_CARGO_BIN", &broken_str),
            ("RUSTFLAGS", "--cfg trampoline_test"),
        ],
        ["--no-cache", "cargo", "run"],
    );
    assert!(
        !out.status.success(),
        "RUSTFLAGS change should force fall-through"
    );
}

#[test]
fn cargo_config_rustflags_edit_forces_fall_through() {
    // Regression test for issue #346: editing `.cargo/config.toml`
    // [build] rustflags must bust the trampoline fingerprint even when
    // the env var `RUSTFLAGS` is unchanged.
    let project = make_project("trampoline-cargo-config-rustflags");
    // Seed: cold build with no .cargo/config.toml — sidecar records a
    // fingerprint that includes the empty-config digest.
    let cold = run_cold(&project, &[]);
    assert!(
        cold.status.success(),
        "seed cold build failed:\n{}",
        String::from_utf8_lossy(&cold.stderr)
    );

    // Add `.cargo/config.toml` declaring new rustflags. After this edit
    // the trampoline must not fast-path the stale binary; if it does,
    // the broken-cargo stub never runs and the assertion below fails.
    let cargo_dir = project.join(".cargo");
    fs::create_dir_all(&cargo_dir).expect("create .cargo");
    fs::write(
        cargo_dir.join("config.toml"),
        "[build]\nrustflags = [\"-C\", \"opt-level=0\"]\n",
    )
    .expect("write .cargo/config.toml");

    let stub_dir = unique_temp_dir("trampoline-cargo-config-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run"],
    );
    assert!(
        !out.status.success(),
        ".cargo/config.toml [build] rustflags edit should force trampoline fall-through; broken cargo must be invoked\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn release_profile_has_distinct_sidecar() {
    let project = make_project("trampoline-release");
    // Cold debug build.
    let cold_debug = run_cold(&project, &[]);
    assert!(cold_debug.status.success(), "cold debug build failed");
    // Cold release build.
    let cold_release = run_cold(&project, &["--release"]);
    assert!(cold_release.status.success(), "cold release build failed");

    let debug_sidecar = project_sidecar(&project, "trampoline_demo", "debug");
    let release_sidecar = project_sidecar(&project, "trampoline_demo", "release");
    assert!(debug_sidecar.is_file(), "debug sidecar missing");
    assert!(release_sidecar.is_file(), "release sidecar missing");

    // Both warm paths should bypass cargo.
    let stub_dir = unique_temp_dir("trampoline-release-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let warm_debug = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run"],
    );
    assert!(
        warm_debug.status.success(),
        "warm debug should hit trampoline: {}",
        String::from_utf8_lossy(&warm_debug.stderr)
    );
    let warm_release = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run", "--release"],
    );
    assert!(
        warm_release.status.success(),
        "warm release should hit trampoline: {}",
        String::from_utf8_lossy(&warm_release.stderr)
    );
}

#[test]
fn binary_missing_falls_through() {
    let project = make_project("trampoline-no-bin");
    let cold = run_cold(&project, &[]);
    assert!(cold.status.success(), "seed build failed");

    // Remove the binary but keep the sidecar.
    let binary = project_binary(&project, "trampoline_demo", "debug");
    fs::remove_file(&binary).expect("remove binary");
    assert!(!binary.exists());

    let stub_dir = unique_temp_dir("trampoline-no-bin-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run"],
    );
    assert!(
        !out.status.success(),
        "missing binary should force fall-through"
    );
}

#[test]
fn stale_fingerprint_falls_through() {
    let project = make_project("trampoline-stale-fp");
    let cold = run_cold(&project, &[]);
    assert!(cold.status.success(), "seed build failed");

    // Corrupt the sidecar's fingerprint by hand.
    let sidecar = project_sidecar(&project, "trampoline_demo", "debug");
    let text = fs::read_to_string(&sidecar).expect("read sidecar");
    let corrupted = text.replace("blake3:", "blake3:deadbeefdeadbeef");
    assert_ne!(corrupted, text, "sidecar fingerprint pattern not found");
    fs::write(&sidecar, corrupted).expect("write corrupted sidecar");

    let stub_dir = unique_temp_dir("trampoline-stale-fp-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run"],
    );
    assert!(
        !out.status.success(),
        "stale fingerprint should force fall-through"
    );
}

#[test]
fn missing_dep_info_falls_through_silently() {
    let project = make_project("trampoline-no-dep");
    let cold = run_cold(&project, &[]);
    assert!(cold.status.success(), "seed build failed");

    let sidecar = project_sidecar(&project, "trampoline_demo", "debug");
    fs::remove_file(&sidecar).expect("remove sidecar");

    // Second invocation: no sidecar → falls through; cargo recompiles
    // (it's a no-op since artifacts exist) and the sidecar should be
    // rewritten.
    let warm = run_cold(&project, &[]);
    assert!(
        warm.status.success(),
        "missing-sidecar fall-through failed: {}",
        String::from_utf8_lossy(&warm.stderr)
    );
    assert!(
        sidecar.is_file(),
        "sidecar should be regenerated after fall-through"
    );
}

#[test]
fn trailing_args_pass_through_to_binary() {
    let project = make_project("trampoline-trailing-args");
    let cold = run_cold(&project, &["--", "alpha", "beta"]);
    assert!(cold.status.success(), "cold trailing-args failed");
    let stdout = String::from_utf8_lossy(&cold.stdout);
    assert!(
        stdout.contains("hello alpha beta"),
        "cold path missed trailing args: {stdout}"
    );

    // Warm path.
    let stub_dir = unique_temp_dir("trampoline-trailing-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let warm = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run", "--", "gamma", "delta"],
    );
    let stdout = String::from_utf8_lossy(&warm.stdout);
    assert!(
        warm.status.success() && stdout.contains("hello gamma delta"),
        "warm path missed trailing args: stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&warm.stderr)
    );
}
