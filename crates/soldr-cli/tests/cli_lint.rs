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

fn dependency_failure_script() -> &'static str {
    #[cfg(windows)]
    {
        "@echo off\nif \"%~1\"==\"audit\" exit /b 9\nping -n 11 127.0.0.1 > nul\nexit /b 0\n"
    }
    #[cfg(not(windows))]
    {
        "#!/bin/sh\nif [ \"$1\" = audit ]; then exit 9; fi\nsleep 10\n"
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

        let output = isolated_soldr_command()
            .args(["--no-cache", "lint", "rust", "--package", "soldr-cli"])
            .env("SOLDR_CACHE_DIR", root.join("cache"))
            .env("SOLDR_TEST_CARGO_BIN", cargo)
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
            read_logged_cargo_invocations(&log),
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
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "dependency failure must cancel sibling lint children promptly"
    );
});
