//! soldr#3407 / zccache#1683: the cache-hit delivery mode reaches the cargo
//! child as `ZCCACHE_MODE`, resolved as `--zccache-mode` /
//! `SOLDR_ZCCACHE_MODE` > `[zccache] mode` > the user's own `ZCCACHE_MODE`.

use crate::common::*;
use std::fs;
use std::process::Output;

struct Case {
    label: &'static str,
    flag: Option<&'static str>,
    soldr_env: Option<&'static str>,
    config: Option<&'static str>,
    user_env: Option<&'static str>,
}

fn run(case: &Case) -> (Output, String) {
    let cache_root = unique_temp_dir(&format!("cargo-zccache-mode-{}", case.label));
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    if let Some(mode) = case.config {
        fs::write(
            cache_root.join("config.toml"),
            format!("[zccache]\nmode = \"{mode}\"\n"),
        )
        .expect("write config.toml");
    }
    let mut command = isolated_soldr_command();
    if let Some(flag) = case.flag {
        command.args(["--zccache-mode", flag]);
    }
    command
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_ZCCACHE_MODE")
        .env_remove("ZCCACHE_MODE");
    if let Some(value) = case.soldr_env {
        command.env("SOLDR_ZCCACHE_MODE", value);
    }
    if let Some(value) = case.user_env {
        command.env("ZCCACHE_MODE", value);
    }
    let output = command.output().expect("run soldr cargo build");
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    (output, log)
}

fn assert_child_mode(case: Case, expected: &str) {
    let (output, log) = run(&case);
    assert!(
        output.status.success(),
        "{}: soldr cargo build failed\nstdout:\n{}\nstderr:\n{}",
        case.label,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        child_mode(&log).as_deref(),
        Some(expected),
        "{}: the cargo child must see ZCCACHE_MODE={expected:?}: {log}",
        case.label
    );
}

/// The `ZCCACHE_MODE` the managed (`cache=1`) cargo build logged; it is the
/// last field. Unwrapped preflight cargo calls are not the build. An unset
/// variable logs empty under sh and as the literal `%ZCCACHE_MODE%` under cmd.
fn child_mode(log: &str) -> Option<String> {
    let line = log
        .lines()
        .find(|line| line.starts_with("cargo wrapper=") && line.contains(" cache=1 "))?;
    let value = line.split_once("zccache_mode=")?.1.trim_end();
    Some(if value.starts_with('%') { "" } else { value }.to_string())
}

#[test]
fn the_flag_wins_over_config_and_the_users_own_variable() {
    assert_child_mode(
        Case {
            label: "flag",
            flag: Some("copy"),
            soldr_env: None,
            config: Some("reflink"),
            user_env: Some("link"),
        },
        "COPY",
    );
}

#[test]
fn soldr_env_wins_over_config() {
    assert_child_mode(
        Case {
            label: "soldr-env",
            flag: None,
            soldr_env: Some("reflink"),
            config: Some("link"),
            user_env: None,
        },
        "REFLINK",
    );
}

#[test]
fn config_toml_applies_over_the_users_own_variable() {
    assert_child_mode(
        Case {
            label: "config",
            flag: None,
            soldr_env: None,
            config: Some("Link"),
            user_env: Some("copy"),
        },
        "LINK",
    );
}

#[test]
fn the_users_own_variable_passes_through_untouched() {
    assert_child_mode(
        Case {
            label: "user",
            flag: None,
            soldr_env: None,
            config: None,
            user_env: Some("reflink"),
        },
        "reflink",
    );
}

#[test]
fn nothing_configured_leaves_zccache_mode_unset() {
    assert_child_mode(
        Case {
            label: "unset",
            flag: None,
            soldr_env: None,
            config: None,
            user_env: None,
        },
        "",
    );
}

#[test]
fn an_invalid_soldr_value_fails_the_build_naming_its_source() {
    let (output, log) = run(&Case {
        label: "invalid",
        flag: None,
        soldr_env: Some("hardlink"),
        config: None,
        user_env: None,
    });
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "an invalid mode must fail: {stderr}"
    );
    assert!(stderr.contains("SOLDR_ZCCACHE_MODE"), "{stderr}");
    assert!(stderr.contains("\"hardlink\""), "{stderr}");
    assert_eq!(
        child_mode(&log),
        None,
        "the managed cargo build must not run: {log}"
    );
}
