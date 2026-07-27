#![allow(unused_imports)]

mod common;

use common::*;
use soldr_cli::timed_test;
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

fn successful_tool_script() -> &'static str {
    #[cfg(windows)]
    {
        "@echo off\nexit /b 0\n"
    }
    #[cfg(not(windows))]
    {
        "#!/bin/sh\nexit 0\n"
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

/// How long an *uncancelled* sibling lint child would run for
/// (soldr#1876).
///
/// This is the negative signal: if cancellation regresses, the run takes at
/// least this long. It is deliberately far above
/// [`SIBLING_CANCEL_BUDGET_SECS`] so the two outcomes cannot be confused on a
/// loaded runner. Raising it costs nothing on the passing path — the siblings
/// are killed almost immediately — and only lengthens an already-failing run.
const SIBLING_SLEEP_SECS: u32 = 30;

/// Wall-clock budget for the whole `lint deps` invocation once the dependency
/// failure has cancelled its siblings.
///
/// Must stay well under [`SIBLING_SLEEP_SECS`] to remain a real assertion,
/// while leaving room for soldr startup, the metadata probe, three fake-script
/// spawns, and teardown. The previous 5 s against a 10 s sleep was only a 2x
/// margin and flaked on `target-run x86_64-pc-windows-msvc` at 5.247 s.
const SIBLING_CANCEL_BUDGET_SECS: u64 = 15;

fn dependency_failure_script() -> &'static str {
    // `ping -n <N>` waits N-1 intervals, so N = SIBLING_SLEEP_SECS + 1.
    #[cfg(windows)]
    {
        const _: () = assert!(SIBLING_SLEEP_SECS == 30);
        "@echo off\nif \"%~1\"==\"metadata\" exit /b 0\nif \"%~1\"==\"audit\" exit /b 9\nping -n 31 127.0.0.1 > nul\nexit /b 0\n"
    }
    #[cfg(not(windows))]
    {
        const _: () = assert!(SIBLING_SLEEP_SECS == 30);
        "#!/bin/sh\nif [ \"$1\" = metadata ]; then exit 0; fi\nif [ \"$1\" = audit ]; then exit 9; fi\nsleep 30\n"
    }
}

timed_test!(lint_deps_runs_all_tools_without_compiler_cache, {
    let root = unique_temp_dir("lint-deps");
    let log = root.join("cargo.log");
    let cargo = install_logging_fake_cargo(&log);
    let tools = root.join("tools");
    fs::create_dir_all(&tools).expect("create fake tool dir");
    for subcommand in ["deny", "audit", "machete"] {
        let tool = fake_script_path(&tools, &format!("cargo-{subcommand}"));
        write_fake_script(&tool, successful_tool_script());
    }

    let output = isolated_soldr_command()
        .args(["lint", "deps"])
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        .env("SOLDR_TEST_CARGO_BIN", cargo)
        .env("PATH", prepend_to_path(&tools))
        .output()
        .expect("run soldr lint deps");

    assert!(
        output.status.success(),
        "lint deps failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let invocations = read_logged_cargo_invocations(&log);
    for expected in [
        vec!["deny".to_string(), "check".to_string()],
        vec!["audit".to_string()],
        vec!["machete".to_string()],
    ] {
        assert!(
            invocations.contains(&expected),
            "expected dependency check {expected:?}; got {invocations:?}"
        );
    }
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("cache daemon"),
        "dependency-only lint must not start the compiler cache"
    );
});

timed_test!(
    lint_rust_uses_canonical_commands_without_a_redundant_check,
    {
        let root = unique_temp_dir("lint-rust");
        let log = root.join("cargo.log");
        let cargo = install_logging_fake_cargo(&log);
        let tools = root.join("tools");
        fs::create_dir_all(&tools).expect("create fake tool dir");
        let dylint = fake_script_path(&tools, "cargo-dylint");
        write_fake_script(&dylint, successful_tool_script());
        let dylint_link = fake_script_path(&tools, "dylint-link");
        write_fake_script(&dylint_link, successful_tool_script());
        let dylint_channel = format!(
            "nightly-2026-05-26-{}",
            soldr_cli::pyo3_detect::host_triple()
        );
        let dylint_release = "1.89.0-nightly";
        let dylint_commit = "0123456789abcdef0123456789abcdef01234567";
        let dylint_identity = format!("{dylint_channel}|{dylint_release}|{dylint_commit}");
        let rustc = install_versioned_fake_rustc(
            "rustc 1.89.0-nightly (0123456789abcdef0123456789abcdef01234567 2026-05-26)",
        );

        let output = isolated_soldr_command()
            .args(["--no-cache", "lint", "rust", "--package", "soldr-cli"])
            .env("SOLDR_CACHE_DIR", root.join("cache"))
            .env("SOLDR_TEST_CARGO_BIN", cargo)
            .env("SOLDR_TEST_RUSTC_BIN", rustc)
            .env("SOLDR_DYLINT_CONFIGURED_TOOLCHAIN", dylint_channel)
            .env("SOLDR_DYLINT_CONFIGURED_RUSTC_RELEASE", dylint_release)
            .env("SOLDR_DYLINT_CONFIGURED_RUSTC_COMMIT_HASH", dylint_commit)
            .env("SOLDR_DYLINT_PREPARED_IDENTITY", dylint_identity)
            .env("PATH", prepend_to_path(&tools))
            .output()
            .expect("run soldr lint rust");

        assert!(
            output.status.success(),
            "lint rust failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            read_logged_cargo_invocations(&log)
                .into_iter()
                .filter(|args| {
                    args.first()
                        .is_none_or(|subcommand| subcommand != "metadata")
                })
                .collect::<Vec<_>>(),
            vec![
                strings(&["fmt", "--all", "--package", "soldr-cli", "--", "--check",]),
                strings(&[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--package",
                    "soldr-cli",
                    "--",
                    "-D",
                    "warnings",
                ]),
                strings(&[
                    "dylint",
                    "--all",
                    "--",
                    "--workspace",
                    "--all-targets",
                    "--package",
                    "soldr-cli",
                ]),
            ],
            "lint rust must use its canonical fmt/clippy/dylint pipeline"
        );
    }
);

timed_test!(dependency_failure_cancels_sibling_lint_children, {
    let root = unique_temp_dir("lint-deps-cancel");
    let tools = root.join("tools");
    fs::create_dir_all(&tools).expect("create fake tool dir");
    for subcommand in ["deny", "audit", "machete"] {
        let tool = fake_script_path(&tools, &format!("cargo-{subcommand}"));
        write_fake_script(&tool, successful_tool_script());
    }
    let cargo = fake_script_path(&tools, "cargo");
    write_fake_script(&cargo, dependency_failure_script());

    let started = Instant::now();
    let output = isolated_soldr_command()
        .args(["lint", "deps"])
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        .env("SOLDR_TEST_CARGO_BIN", cargo)
        .env("PATH", prepend_to_path(&tools))
        .output()
        .expect("run failing soldr lint deps");

    assert_eq!(output.status.code(), Some(9));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(SIBLING_CANCEL_BUDGET_SECS),
        "dependency failure must cancel sibling lint children promptly: took \
         {elapsed:?}, and an uncancelled sibling would have slept \
         {SIBLING_SLEEP_SECS}s (soldr#1876)"
    );
});
