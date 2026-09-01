//! Integration coverage for the `soldr cook` subcommand introduced in
//! issue #359. Network-touching scenarios (a real `cargo-chef` fetch +
//! cook run against a small example project) are gated behind the
//! `SOLDR_TEST_NETWORK` env var so they do not run in CI by default.
#![allow(unused_imports)]

use crate::common;

use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn help_advertises_cook_subcommand() {
    let output = Command::new(common::soldr_bin())
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
    let output = Command::new(common::soldr_bin())
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
    let output = Command::new(common::soldr_bin())
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
    let output = Command::new(common::soldr_bin())
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
    let output = Command::new(common::soldr_bin())
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
#[ignore = "network-touching; opt in with SOLDR_TEST_NETWORK=1 cargo test --test cook_dylint -- --ignored cli_cook::"]
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
    let lock_status = Command::new(common::cargo_bin())
        .args(["generate-lockfile"])
        .current_dir(tmp.path())
        .status()
        .expect("cargo generate-lockfile must succeed for the smoke test");
    assert!(lock_status.success());

    let recipe = tmp.path().join("recipe.json");
    let output = Command::new(common::soldr_bin())
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

/// Soldr CLI argv that disables zccache so the only thing speeding up the
/// post-cook build is the dependency artifacts cargo-chef left behind in
/// `target/`. Without `--no-cache` zccache would also serve hits on the
/// cold side and muddy the comparison.
const NO_CACHE: &[&str] = &["--no-cache"];

/// End-to-end speedup benchmark for `soldr cook`. Demonstrates the
/// cargo-chef value prop: once `cook` has populated `target/` with the
/// transitive dep artifacts, a subsequent `cargo build` only has to
/// compile the user's own crate — so it is dramatically faster than a
/// from-scratch build that has to compile the whole dep graph too.
///
/// Network + heavy: pulls `serde_json` + its transitive deps from
/// crates.io, downloads the pinned `cargo-chef` release, and runs three
/// full cargo builds. Opt in explicitly:
///
/// ```text
/// SOLDR_TEST_COOK_BENCH=1 \
///   cargo test --test cook_dylint -- --ignored cli_cook::cook_post_build_is_significantly_faster
/// ```
///
/// The assertion is intentionally loose (post-cook build must be at
/// least 2× faster than the cold from-scratch build). Real speedups on
/// developer hardware are typically 5–10× — the 2× floor exists so the
/// test survives slow shared CI workers and Windows Defender overhead.
#[test]
#[ignore = "perf+network; opt in with SOLDR_TEST_COOK_BENCH=1 cargo test --test cook_dylint -- --ignored cli_cook::cook_post_build_is_significantly_faster"]
fn cook_post_build_is_significantly_faster() {
    if std::env::var_os("SOLDR_TEST_COOK_BENCH").is_none() {
        eprintln!("skipping: SOLDR_TEST_COOK_BENCH not set");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let project = tmp.path();

    // A minimal but non-trivial project: serde_json pulls in serde +
    // serde_derive (proc-macro, slow to compile) + itoa + ryu + memchr
    // + serde_core, so the dep compile dominates a from-scratch build.
    // That's exactly where cook's pre-populated `target/` pays off.
    std::fs::write(
        project.join("Cargo.toml"),
        r#"[package]
name = "soldr_cook_bench_demo"
version = "0.0.0"
edition = "2021"

[dependencies]
serde_json = "1"
"#,
    )
    .unwrap();
    let src = project.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("main.rs"),
        r#"fn main() {
    let v: serde_json::Value = serde_json::json!({ "hello": "cold" });
    println!("{}", v);
}
"#,
    )
    .unwrap();

    // Materialise Cargo.lock once up front so neither side pays the
    // resolver cost during the timed build.
    let lock_status = Command::new(common::cargo_bin())
        .args(["generate-lockfile"])
        .current_dir(project)
        .status()
        .expect("cargo generate-lockfile must succeed");
    assert!(lock_status.success(), "generate-lockfile failed");

    // Pre-fetch dep sources so the timed builds don't include network
    // download time — we're measuring compile, not curl.
    let fetch_status = Command::new(common::cargo_bin())
        .args(["fetch"])
        .current_dir(project)
        .status()
        .expect("cargo fetch must succeed");
    assert!(fetch_status.success(), "cargo fetch failed");

    // === Scenario A: cold build from scratch (no cook) =================
    wipe_target(project);
    let (t_cold, cold_status) = time_soldr(project, NO_CACHE, "cargo build --release");
    assert!(
        cold_status,
        "cold `soldr --no-cache cargo build --release` failed"
    );

    // === Scenario B: cook, then build =================================
    wipe_target(project);
    let (t_cook, cook_status) = time_soldr(project, NO_CACHE, "cook --release");
    assert!(cook_status, "`soldr --no-cache cook --release` failed");

    // Touch main.rs to invalidate the prior `cargo build` artefact even
    // though we wiped `target/` — defensive against cook accidentally
    // leaving a binary behind. The dep .rlibs cargo-chef wrote stay put.
    std::fs::write(
        src.join("main.rs"),
        r#"fn main() {
    let v: serde_json::Value = serde_json::json!({ "hello": "post-cook" });
    println!("{}", v);
}
"#,
    )
    .unwrap();

    let (t_post_cook, post_cook_status) = time_soldr(project, NO_CACHE, "cargo build --release");
    assert!(
        post_cook_status,
        "post-cook `soldr --no-cache cargo build --release` failed"
    );

    let ratio = t_post_cook.as_secs_f64() / t_cold.as_secs_f64().max(f64::EPSILON);
    eprintln!(
        "\nsoldr cook speedup summary\n\
         --------------------------\n\
           T_cold      (cargo build, target/ empty)         = {:?}\n\
           T_cook      (soldr cook, target/ empty)          = {:?}\n\
           T_post_cook (cargo build, target/ pre-populated) = {:?}\n\
           ratio       (T_post_cook / T_cold)               = {:.3}\n\
           speedup                                          = {:.2}×\n",
        t_cold,
        t_cook,
        t_post_cook,
        ratio,
        1.0 / ratio,
    );

    // Pre-cooked target/ must make the second build at least 2× faster
    // than a cold from-scratch build. This is the soldr cook value
    // prop: in CI / Docker layered caching, you pay `T_cook` once and
    // amortise it across every subsequent source-only change.
    assert!(
        ratio < 0.5,
        "soldr cook did not deliver a significant speedup: \
         T_post_cook = {:?}, T_cold = {:?}, ratio = {:.3} (want < 0.5, i.e. ≥ 2× faster)",
        t_post_cook,
        t_cold,
        ratio,
    );
}

fn wipe_target(project: &std::path::Path) {
    let target = project.join("target");
    if target.exists() {
        std::fs::remove_dir_all(&target).expect("failed to wipe target dir");
    }
}

/// Run the soldr CLI with the given pre-subcommand flags + a single
/// space-separated tail (`cargo build --release`, `cook --release`,
/// etc.) and return `(wallclock, success)`. The split is just so the
/// caller reads naturally; clap doesn't care about quoting at this
/// level since none of our tokens contain spaces.
fn time_soldr(project: &std::path::Path, leading: &[&str], tail: &str) -> (Duration, bool) {
    let mut cmd = Command::new(common::soldr_bin());
    cmd.args(leading);
    cmd.args(tail.split_whitespace());
    cmd.current_dir(project);

    let started = Instant::now();
    let output = cmd.output().expect("failed to spawn soldr");
    let elapsed = started.elapsed();

    if !output.status.success() {
        eprintln!("soldr {} {}", leading.join(" "), tail);
        eprintln!("  exit: {:?}", output.status);
        eprintln!("  stdout:\n{}", String::from_utf8_lossy(&output.stdout));
        eprintln!("  stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    }
    (elapsed, output.status.success())
}
