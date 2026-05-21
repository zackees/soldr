//! Integration coverage for the `soldr cook` subcommand introduced in
//! issue #359. Network-touching scenarios (a real `cargo-chef` fetch +
//! cook run against a small example project) are gated behind the
//! `SOLDR_TEST_NETWORK` env var so they do not run in CI by default.
#![allow(unused_imports)]

mod common;

use std::process::Command;

#[test]
fn help_advertises_cook_subcommand() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("--help")
        .output()
        .expect("failed to run soldr --help");

    assert!(output.status.success(), "help command failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cook"),
        "soldr --help must advertise the cook subcommand"
    );
}

#[test]
fn cook_help_describes_supported_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cook", "--help"])
        .output()
        .expect("failed to run soldr cook --help");

    assert!(output.status.success(), "cook --help should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The clap auto-generated help should at least include the long-form
    // doc comment summary. We assert on `cargo-chef` so the prose
    // describing the shim survives any future help reformatting.
    assert!(
        stdout.contains("cargo-chef") || stdout.contains("cargo chef"),
        "cook --help must reference cargo-chef so users learn what powers it; got:\n{stdout}"
    );
}

#[test]
fn cook_fails_when_no_cargo_toml_above_cwd() {
    // Run with a cwd that has no Cargo.toml at any depth above it. We pick
    // tempfile::tempdir which is usually under the system temp root — no
    // ancestor will contain Cargo.toml.
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("cook")
        .arg("--prepare-only")
        .current_dir(tmp.path())
        .output()
        .expect("failed to run soldr cook");

    assert!(
        !output.status.success(),
        "cook without a Cargo.toml must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no Cargo.toml") || stderr.contains("Cargo.toml"),
        "stderr should explain the missing manifest; got:\n{stderr}"
    );
}

#[test]
fn cook_rejects_cook_only_without_recipe_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"smoke\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cook", "--cook-only"])
        .current_dir(tmp.path())
        .output()
        .expect("failed to run soldr cook --cook-only");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--cook-only") && stderr.contains("--recipe-path"),
        "expected mutual-requirement error; got:\n{stderr}"
    );
}

#[test]
fn cook_rejects_unknown_flag_before_passthrough_separator() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"smoke\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cook", "--definitely-not-a-flag"])
        .current_dir(tmp.path())
        .output()
        .expect("failed to run soldr cook --definitely-not-a-flag");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown flag"),
        "expected an unknown-flag error; got:\n{stderr}"
    );
}

#[test]
#[ignore = "network-touching; opt in with SOLDR_TEST_NETWORK=1 cargo test --test cli_cook -- --ignored"]
fn cook_prepare_only_against_example_project_runs_end_to_end() {
    if std::env::var_os("SOLDR_TEST_NETWORK").is_none() {
        eprintln!("skipping: SOLDR_TEST_NETWORK not set");
        return;
    }

    // A bare-minimum cargo project so cargo-chef has something to read.
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"smoke\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();

    // Force cargo to materialise Cargo.lock so the recipe has lock data.
    let lock_status = Command::new("cargo")
        .args(["generate-lockfile"])
        .current_dir(tmp.path())
        .status()
        .expect("cargo generate-lockfile must succeed for the smoke test");
    assert!(lock_status.success());

    let recipe = tmp.path().join("recipe.json");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cook", "--prepare-only", "--recipe-path"])
        .arg(&recipe)
        .current_dir(tmp.path())
        .output()
        .expect("failed to run soldr cook --prepare-only");
    if !output.status.success() {
        eprintln!("stdout:\n{}", String::from_utf8_lossy(&output.stdout));
        eprintln!("stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(output.status.success());
    assert!(recipe.is_file(), "cargo chef prepare must write the recipe");
}
