//! Who consumes a jobs flag under `soldr build` (soldr#2786).
//!
//! The `--jobs` long_help documents how an explicit caller value reaches
//! Cargo. If the routing ever changes, that text becomes a wrong answer, so it
//! is pinned here rather than trusted.
//!
//! Measured, not assumed. Each row was run and its exit code recorded before
//! being written down:
//!
//! | invocation                          | consumed by | exit |
//! |-------------------------------------|-------------|------|
//! | `soldr --jobs N build`              | soldr       | 2    |
//! | `soldr build --jobs N`              | soldr       | 2    |
//! | `soldr build -j N`                  | cargo       | 101  |
//! | `soldr build -- --jobs N`           | cargo       | 101  |
//! | `CARGO_BUILD_JOBS=N soldr build`    | cargo       | 101  |
//!
//! The tell is which parser rejects a non-numeric value: clap says
//! `invalid value 'x' for '--jobs <N>'`, cargo says `could not parse`.

use crate::common;

use std::process::Command;

fn fixture() -> std::path::PathBuf {
    let dir = common::unique_temp_dir("jobs-routing");
    std::fs::create_dir_all(dir.join("src")).expect("create src");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"jobsroute\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
    dir
}

fn run(args: &[&str], env: Option<(&str, &str)>) -> String {
    let dir = fixture();
    let mut cmd = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut cmd);
    cmd.args(args)
        .current_dir(&dir)
        .env("SOLDR_ALLOW_UNPINNED", "1");
    if let Some((k, v)) = env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run soldr");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// clap rejected it, so the flag never left soldr.
fn rejected_by_soldr(text: &str) -> bool {
    text.contains("invalid value") && text.contains("--jobs")
}

/// cargo rejected it, so the flag reached cargo.
fn rejected_by_cargo(text: &str) -> bool {
    text.contains("could not parse") || text.contains("Number of parallel jobs")
}

#[test]
fn long_jobs_is_soldrs_at_every_position() {
    for args in [
        vec!["--jobs", "notanumber", "build"],
        vec!["build", "--jobs", "notanumber"],
    ] {
        let text = run(&args, None);
        assert!(
            rejected_by_soldr(&text),
            "`soldr {}` must be soldr's own flag; got:\n{text}",
            args.join(" ")
        );
    }
}

// The three explicit routes documented by `--jobs`. If any stops reaching
// Cargo, that contract is wrong and this fails.
#[test]
fn short_j_reaches_cargo() {
    let text = run(&["build", "-j", "notanumber"], None);
    assert!(
        rejected_by_cargo(&text),
        "`-j` must reach cargo; got:\n{text}"
    );
}

#[test]
fn double_dash_jobs_reaches_cargo() {
    let text = run(&["build", "--", "--jobs", "notanumber"], None);
    assert!(
        rejected_by_cargo(&text),
        "`-- --jobs` must reach cargo; got:\n{text}"
    );
}

#[test]
fn cargo_build_jobs_env_reaches_cargo() {
    let text = run(&["build"], Some(("CARGO_BUILD_JOBS", "notanumber")));
    assert!(
        rejected_by_cargo(&text),
        "CARGO_BUILD_JOBS must reach cargo under `soldr build`; got:\n{text}"
    );
}

// The control soldr#2786 used to argue the gap: `soldr cargo build` honours
// the variable. Keeping both in one file is what makes a future divergence
// between the two surfaces visible.
#[test]
fn cargo_front_door_honours_the_env_too() {
    let text = run(
        &["cargo", "build"],
        Some(("CARGO_BUILD_JOBS", "notanumber")),
    );
    assert!(
        rejected_by_cargo(&text),
        "`soldr cargo build` must honour CARGO_BUILD_JOBS; got:\n{text}"
    );
}
