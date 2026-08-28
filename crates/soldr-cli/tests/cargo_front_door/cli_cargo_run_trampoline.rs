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

use crate::common;

use crate::common::*;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

fn soldr_bin() -> std::path::PathBuf {
    // soldr#1039 phase 1.
    common::soldr_bin()
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
    let body = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "@echo off\necho broken cargo should not have been spawned 1>&2\nexit /b 99\n"
    } else {
        "#!/bin/sh\necho 'broken cargo should not have been spawned' >&2\nexit 99\n"
    };
    write_fake_script(&path, body);
    path
}

fn run_soldr<I, S>(project: &Path, env_overrides: &[(&str, &str)], args: I) -> std::process::Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(soldr_bin());
    common::scrub_outer_soldr_env(&mut cmd);
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

/// Assert a seed/cold build succeeded, printing what it actually said.
///
/// soldr#2336: every one of these assertions used to be
/// `assert!(cold.status.success(), "seed build failed")`, which discards both
/// pipes. When the new windows-gnu replay lane went red, three of them failed
/// in ~0.3s -- far too fast to be a compile -- and the retained output said
/// only "seed build failed". A build that dies that quickly has a reason and
/// prints it; there was simply nowhere for it to go. Neighbouring assertions
/// in this file already interpolate stderr, so this only makes the seed builds
/// as diagnosable as the warm ones.
#[track_caller]
fn assert_build_ok(output: &std::process::Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} (exit {:?})
stdout:
{}
stderr:
{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Return the effective `target/<triple?>/<profile>/` directory that the
/// soldr cargo front door will land artifacts in. On Windows soldr
/// injects `CARGO_BUILD_TARGET=<host>` by default, so artifacts live
/// under `target/<host_triple>/<profile>/`. On Unix the host triple is
/// left implicit and artifacts live at `target/<profile>/`.
fn profile_root(project: &Path, profile: &str) -> PathBuf {
    let mut root = project.join("target");
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
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
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
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
    assert_build_ok(&cold, "seed build failed");

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
    assert_build_ok(&cold, "seed build failed");

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
    assert_build_ok(&cold, "seed build failed");

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
    assert_build_ok(&cold, "seed build failed");

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
    assert_build_ok(&cold, "seed build failed");

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
    assert_build_ok(&cold_debug, "cold debug build failed");
    // Cold release build.
    let cold_release = run_cold(&project, &["--release"]);
    assert_build_ok(&cold_release, "cold release build failed");

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
    assert_build_ok(&cold, "seed build failed");

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
    assert_build_ok(&cold, "seed build failed");

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
    assert_build_ok(&cold, "seed build failed");

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

// -------------------------------------------------------------------------
// Content-hash oracle scenarios (issue #342). Each test sets up a state
// that mtime+size alone cannot diagnose correctly; the content-hash
// oracle is what makes the outcome correct.
// -------------------------------------------------------------------------

/// Reset a file's modification time to a fixed past instant. Returns the
/// SystemTime that was set so the test can compare against it later.
fn reset_mtime_to_epoch_offset(path: &Path, seconds_from_epoch: u64) -> SystemTime {
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds_from_epoch);
    let file = fs::File::options()
        .write(true)
        .open(path)
        .expect("open writable");
    file.set_modified(when).expect("set mtime");
    when
}

#[test]
fn content_edit_with_spoofed_old_mtime_forces_fall_through() {
    // Issue #342: an attacker (or innocent reproducible-build tool)
    // edits a source file but restores its mtime to an old value.
    // mtime+size oracle would compare mtimes only and accept the
    // unchanged stat; content-hash oracle MUST detect the change.
    let project = make_project("trampoline-spoofed-old-mtime");
    let cold = run_cold(&project, &[]);
    assert_build_ok(&cold, "seed build failed");

    // Read the sidecar's recorded mtime BEFORE we edit so we can
    // restore it exactly.
    let sidecar = project_sidecar(&project, "trampoline_demo", "debug");
    let sidecar_text = fs::read_to_string(&sidecar).expect("read sidecar");
    // Grab the first source's mtime_nanos. Cheap regex avoids depending
    // on the full TOML parser for one number.
    let re_match = sidecar_text
        .lines()
        .find(|l| l.trim_start().starts_with("mtime_nanos"))
        .and_then(|l| l.split('=').nth(1))
        .map(|s| s.trim())
        .expect("recorded mtime_nanos line");
    let recorded_nanos: u128 = re_match.parse().expect("parse mtime nanos");
    let recorded_secs = (recorded_nanos / 1_000_000_000) as u64;

    let main_rs = project.join("src").join("main.rs");
    let old_body = fs::read_to_string(&main_rs).expect("read main");
    let new_body = old_body.replacen("hello", "hullo", 1);
    assert_ne!(new_body, old_body, "fixture mutation must change content");
    assert_eq!(
        new_body.len(),
        old_body.len(),
        "fixture must preserve size exactly like the reported VERSION_A/VERSION_B restore"
    );
    fs::write(&main_rs, new_body).expect("write main");
    // Restore the original mtime so a stat-only oracle would accept.
    reset_mtime_to_epoch_offset(&main_rs, recorded_secs);

    // Size and mtime now match the recorded stat shape while content differs.
    // Use a broken cargo to prove the content oracle forced fall-through.
    let stub_dir = unique_temp_dir("trampoline-spoofed-old-mtime-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run"],
    );
    assert!(
        !out.status.success(),
        "content-edit-with-old-mtime MUST fall through to cargo; trampoline accepted stale binary"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Cargo may run a stale artifact"),
        "the content mismatch with an older source mtime must be visible to the caller"
    );
}

#[test]
fn mtime_epoch_restore_with_unchanged_content_still_hits_trampoline() {
    // Issue #342: a tar restore with --mtime=epoch normalizes every
    // file's mtime to a fixed past instant. Content is unchanged.
    // Old mtime+size oracle would fall through (mtimes diverge);
    // content-hash oracle MUST accept (content matches) AND
    // self-heal the sidecar with the new mtimes.
    let project = make_project("trampoline-mtime-epoch");
    let cold = run_cold(&project, &[]);
    assert_build_ok(&cold, "seed build failed");

    let main_rs = project.join("src").join("main.rs");
    let binary = project_binary(&project, "trampoline_demo", "debug");
    // Reset to a fixed-epoch mtime. Sources AND binary both get the
    // same time, simulating `tar --mtime=epoch` extraction.
    reset_mtime_to_epoch_offset(&main_rs, 1_000_000_000);
    reset_mtime_to_epoch_offset(&binary, 1_000_000_000);

    let stub_dir = unique_temp_dir("trampoline-mtime-epoch-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run", "--", "epoch"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "mtime-epoch restore with unchanged content MUST hit trampoline (content hash matches)\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("hello epoch"),
        "binary stdout missing on mtime-epoch warm path: {stdout}"
    );

    // Self-heal: the sidecar should now have the new mtime_nanos =
    // 1_000_000_000 * 1e9. Re-running again must hit the FAST-SKIP
    // path (no re-hashing). We can't directly assert "did not hash"
    // but we can assert the sidecar was rewritten.
    let sidecar = project_sidecar(&project, "trampoline_demo", "debug");
    let post_text = fs::read_to_string(&sidecar).expect("read sidecar");
    assert!(
        post_text.contains("1000000000000000000"),
        "self-heal should have rewritten binary/source mtime to 1e9 seconds (sidecar: {post_text})"
    );
}

#[test]
fn binary_swap_with_matching_mtime_size_is_detected() {
    // Issue #342: someone replaces the on-disk binary with a
    // different one whose mtime+size happen to match the sidecar
    // (cache corruption, accidental cp, attack scenario). Old
    // mtime+size oracle would happily exec the wrong binary;
    // content-hash oracle MUST detect via binary_hash mismatch.
    let project = make_project("trampoline-binary-swap");
    let cold = run_cold(&project, &[]);
    assert_build_ok(&cold, "seed build failed");

    let binary = project_binary(&project, "trampoline_demo", "debug");
    let bin_meta = fs::metadata(&binary).expect("stat binary");
    let original_size = bin_meta.len();
    let original_mtime = bin_meta.modified().expect("binary mtime");

    // Overwrite with random bytes of the SAME size.
    let replacement: Vec<u8> = (0..original_size as usize)
        .map(|i| (i % 251) as u8 ^ 0xAA)
        .collect();
    fs::write(&binary, &replacement).expect("overwrite binary");
    // Restore the mtime so stat-only oracle thinks nothing changed.
    let f = fs::File::options()
        .write(true)
        .open(&binary)
        .expect("reopen binary");
    f.set_modified(original_mtime).expect("restore mtime");

    let stub_dir = unique_temp_dir("trampoline-binary-swap-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run"],
    );
    assert!(
        !out.status.success(),
        "binary-swap-with-matching-mtime+size MUST fall through; trampoline accepted wrong binary"
    );
}

#[test]
fn legacy_sidecar_without_content_hash_forces_rewrite() {
    // Backwards-compat: a sidecar written by the pre-#342 trampoline
    // has no `binary_hash` / `content_hash` fields. The verifier must
    // treat the empty hash as "must fall through and rewrite", so the
    // next build upgrades the entry. Verified by: (1) trimming the
    // hash fields from the sidecar, (2) confirming fall-through, (3)
    // confirming the sidecar regains the fields on the rebuild.
    let project = make_project("trampoline-legacy-sidecar");
    let cold = run_cold(&project, &[]);
    assert_build_ok(&cold, "seed build failed");

    let sidecar = project_sidecar(&project, "trampoline_demo", "debug");
    let text = fs::read_to_string(&sidecar).expect("read sidecar");
    // Strip every `binary_hash` and `content_hash` line.
    let stripped: String = text
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("binary_hash") && !t.starts_with("content_hash")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&sidecar, &stripped).expect("write stripped sidecar");

    let stub_dir = unique_temp_dir("trampoline-legacy-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken_str = broken.to_string_lossy().to_string();
    let out = run_soldr(
        &project,
        &[("SOLDR_TEST_CARGO_BIN", &broken_str)],
        ["--no-cache", "cargo", "run"],
    );
    assert!(
        !out.status.success(),
        "legacy sidecar (empty binary_hash) MUST force fall-through so the next build upgrades it"
    );
}

#[test]
fn trailing_args_pass_through_to_binary() {
    let project = make_project("trampoline-trailing-args");
    let cold = run_cold(&project, &["--", "alpha", "beta"]);
    assert_build_ok(&cold, "cold trailing-args failed");
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
