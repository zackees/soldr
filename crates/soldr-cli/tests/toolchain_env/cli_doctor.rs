#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::io::Write;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// of the three rustup subcommands invoked by `soldr doctor`. Lines are
/// written verbatim into the corresponding response files.
struct DoctorFakeRustupBehavior {
    toolchain_list: Vec<String>,
    components_installed: Vec<String>,
    targets_installed: Vec<String>,
}

/// Fake rustup that branches on argv to satisfy `soldr doctor` queries:
/// - `toolchain list` echoes one line per `behavior.toolchain_list`
/// - `component list --installed --toolchain <ch>` echoes one line per
///   `behavior.components_installed`
/// - `target list --installed --toolchain <ch>` echoes one line per
///   `behavior.targets_installed`
///
/// Any other invocation prints an error and exits 1 (the test must fail).
/// Each invocation is also logged to `log_path` like the logging fake.
fn install_doctor_fake_rustup(log_path: &Path, behavior: &DoctorFakeRustupBehavior) -> PathBuf {
    let dir = unique_temp_dir("fake-rustup-doctor");
    let toolchain_list_path = dir.join("toolchain_list.txt");
    let components_path = dir.join("components.txt");
    let targets_path = dir.join("targets.txt");
    fs::write(&toolchain_list_path, behavior.toolchain_list.join("\n"))
        .expect("failed to write fake toolchain list");
    fs::write(&components_path, behavior.components_installed.join("\n"))
        .expect("failed to write fake components list");
    fs::write(&targets_path, behavior.targets_installed.join("\n"))
        .expect("failed to write fake targets list");

    let rustup = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        dir.join("rustup.bat")
    } else {
        fake_script_path(&dir, "rustup")
    };

    let script = doctor_fake_rustup_script(
        log_path,
        &toolchain_list_path,
        &components_path,
        &targets_path,
    );
    write_fake_script(&rustup, &script);
    rustup
}

fn doctor_fake_rustup_script(
    log_path: &Path,
    toolchain_list_path: &Path,
    components_path: &Path,
    targets_path: &Path,
) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             setlocal enabledelayedexpansion\n\
             set \"first=%~1\"\n\
             set \"second=%~2\"\n\
             set \"line=\"\n\
             :loop\n\
             if \"%~1\"==\"\" goto done\n\
             if defined line (set \"line=!line!\u{1f}%~1\") else (set \"line=%~1\")\n\
             shift\n\
             goto loop\n\
             :done\n\
             echo !line!>>\"{log}\"\n\
             if /I \"!first!\"==\"toolchain\" (\n\
               if /I \"!second!\"==\"list\" (\n\
                 type \"{toolchain_list}\"\n\
                 exit /b 0\n\
               )\n\
             )\n\
             if /I \"!first!\"==\"component\" (\n\
               if /I \"!second!\"==\"list\" (\n\
                 type \"{components}\"\n\
                 exit /b 0\n\
               )\n\
             )\n\
             if /I \"!first!\"==\"target\" (\n\
               if /I \"!second!\"==\"list\" (\n\
                 type \"{targets}\"\n\
                 exit /b 0\n\
               )\n\
             )\n\
             echo unsupported rustup invocation 1>&2\n\
             exit /b 1\n",
            log = log_path.display(),
            toolchain_list = toolchain_list_path.display(),
            components = components_path.display(),
            targets = targets_path.display(),
        )
    } else {
        format!(
            "#!/bin/sh\n\
             sep=$(printf '\\037')\n\
             out=\"\"\n\
             first=1\n\
             for arg in \"$@\"; do\n\
               if [ $first -eq 1 ]; then\n\
                 out=\"$arg\"\n\
                 first=0\n\
               else\n\
                 out=\"$out${{sep}}$arg\"\n\
               fi\n\
             done\n\
             printf '%s\\n' \"$out\" >> \"{log}\"\n\
             if [ \"$1\" = \"toolchain\" ] && [ \"$2\" = \"list\" ]; then\n\
               cat \"{toolchain_list}\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"component\" ] && [ \"$2\" = \"list\" ]; then\n\
               cat \"{components}\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"target\" ] && [ \"$2\" = \"list\" ]; then\n\
               cat \"{targets}\"\n\
               exit 0\n\
             fi\n\
             echo \"unsupported rustup invocation: $*\" >&2\n\
             exit 1\n",
            log = log_path.display(),
            toolchain_list = toolchain_list_path.display(),
            components = components_path.display(),
            targets = targets_path.display(),
        )
    }
}

#[test]
fn doctor_reports_drift_when_component_missing() {
    let workspace = unique_temp_dir("doctor-drift-component");
    seed_rust_toolchain_toml(
        &workspace,
        "[toolchain]\n\
         channel = \"1.94.1\"\n\
         components = [\"clippy\"]\n",
    );
    let log_path = workspace.join("rustup.log");
    let rustup = install_doctor_fake_rustup(
        &log_path,
        &DoctorFakeRustupBehavior {
            toolchain_list: vec!["1.94.1-x86_64-unknown-linux-gnu (default)".to_string()],
            components_installed: Vec::new(),
            targets_installed: vec!["x86_64-unknown-linux-gnu".to_string()],
        },
    );

    // Isolate SOLDR_CACHE_DIR / HOME / USERPROFILE so the doctor's
    // zccache-bundle probe (which inspects `~/.soldr/bin/zccache-pinned/`)
    // can't trip on a stale pinned install in the host's real
    // `~/.soldr/`. Without this, a stale pinned-zccache `source.json`
    // on the dev machine causes `soldr doctor` to exit 1 with empty
    // stdout — a real bug worth fixing in `resolve_pinned_zccache`
    // separately, but the test should be hermetic regardless.
    let output = Command::new(common::soldr_bin())
        .args(["doctor", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_CACHE_DIR", &workspace)
        .env("HOME", &workspace)
        .env("USERPROFILE", &workspace)
        .output()
        .expect("failed to run soldr doctor --json");

    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor must exit 1 when drift detected\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("doctor --json must produce JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "doctor");
    assert_eq!(json["toolchain"]["channel"], "1.94.1");
    assert_eq!(json["toolchain"]["installed"], true);
    assert_eq!(json["drift"], true);
    assert_eq!(
        json["missing_components"],
        serde_json::json!(["clippy"]),
        "missing_components must list clippy"
    );
    assert_eq!(
        json["missing_targets"],
        serde_json::json!([]),
        "no targets declared so no targets missing"
    );
    // soldr#1838 Phase 4: the compile-daemon fallback rollup is always
    // present (empty when the cache was never bypassed), so a consumer can
    // read it unconditionally.
    assert!(
        json["fallbacks"]["total"].is_number(),
        "doctor --json must carry a fallbacks rollup: {json}"
    );
    assert!(
        json["fallbacks"]["recent"].is_array(),
        "the fallback rollup must expose a recent[] list: {json}"
    );
}

#[test]
fn doctor_reports_no_drift_when_everything_installed() {
    let workspace = unique_temp_dir("doctor-no-drift");
    seed_rust_toolchain_toml(
        &workspace,
        "[toolchain]\n\
         channel = \"1.94.1\"\n\
         components = [\"clippy\"]\n",
    );
    let log_path = workspace.join("rustup.log");
    // Rustup reports components as target-qualified — soldr doctor must
    // treat `clippy-x86_64-...` as satisfying a declared `clippy`.
    let rustup = install_doctor_fake_rustup(
        &log_path,
        &DoctorFakeRustupBehavior {
            toolchain_list: vec!["1.94.1-x86_64-unknown-linux-gnu (default)".to_string()],
            components_installed: vec!["clippy-x86_64-unknown-linux-gnu".to_string()],
            targets_installed: vec!["x86_64-unknown-linux-gnu".to_string()],
        },
    );

    // Isolate SOLDR_CACHE_DIR / HOME / USERPROFILE so the doctor's
    // zccache-bundle probe (which inspects `~/.soldr/bin/zccache-pinned/`)
    // can't trip on a stale pinned install in the host's real
    // `~/.soldr/`. Without this, a stale pinned-zccache `source.json`
    // on the dev machine causes `soldr doctor` to exit 1 with empty
    // stdout — a real bug worth fixing in `resolve_pinned_zccache`
    // separately, but the test should be hermetic regardless.
    let output = Command::new(common::soldr_bin())
        .args(["doctor", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_CACHE_DIR", &workspace)
        .env("HOME", &workspace)
        .env("USERPROFILE", &workspace)
        .output()
        .expect("failed to run soldr doctor --json");

    assert_eq!(
        output.status.code(),
        Some(0),
        "doctor must exit 0 when no drift\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("doctor --json must produce JSON");
    assert_eq!(json["drift"], false);
    assert_eq!(json["missing_components"], serde_json::json!([]));
    assert_eq!(json["missing_targets"], serde_json::json!([]));
    let components = json["components"].as_array().expect("components array");
    assert_eq!(components.len(), 1);
    assert_eq!(components[0]["name"], "clippy");
    assert_eq!(components[0]["installed"], true);
}

#[test]
fn doctor_handles_missing_manifest() {
    let workspace = unique_temp_dir("doctor-no-manifest");
    let log_path = workspace.join("rustup.log");
    // Failing fake rustup — if doctor invokes it the test fails because
    // rustup will write to the log and exit non-zero.
    let rustup = install_failing_fake_rustup(&log_path);

    let output = Command::new(common::soldr_bin())
        .args(["doctor"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .output()
        .expect("failed to run soldr doctor");

    assert_eq!(
        output.status.code(),
        Some(0),
        "doctor must exit 0 when manifest is missing\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no rust-toolchain.toml"),
        "doctor stdout should mention missing manifest: {stdout}"
    );

    // The failing fake rustup writes to log_path on every invocation, so
    // an empty (or non-existent) log proves rustup was never spawned.
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.trim().is_empty(),
        "doctor must not invoke rustup when no manifest exists; log was: {log}"
    );
}

#[test]
fn issue_2476_doctor_json_reports_broker_deadline_provenance() {
    let workspace = unique_temp_dir("doctor-broker-deadlines");
    let mut command = common::isolated_soldr_command();
    let output = command
        .args(["doctor", "--json"])
        .current_dir(&workspace)
        .env("HOME", &workspace)
        .env("USERPROFILE", &workspace)
        .env("SOLDR_CACHE_DIR", &workspace)
        .env("SOLDR_BROKER_BUSY_BUDGET_MS", "17")
        .env("SOLDR_BROKER_FIRST_RESPONSE_MS", "0")
        .env("SOLDR_BROKER_PROGRESS_SILENCE_MS", "23")
        .env("SOLDR_ROUTE_ACQUIRE_CEILING_MS", "invalid")
        .output()
        .expect("run doctor deadline report");
    let json: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "doctor deadline JSON failed: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let rows = json["broker_deadlines"]
        .as_array()
        .expect("broker_deadlines array");
    let row = |env_var: &str| {
        rows.iter()
            .find(|row| row["env_var"] == env_var)
            .unwrap_or_else(|| panic!("missing deadline row {env_var}"))
    };

    assert!(output.status.success());
    assert_eq!(row("SOLDR_BROKER_BUSY_BUDGET_MS")["effective_ms"], 17);
    assert_eq!(row("SOLDR_BROKER_BUSY_BUDGET_MS")["source"], "override");
    assert_eq!(row("SOLDR_BROKER_FIRST_RESPONSE_MS")["effective_ms"], 2_000);
    assert!(row("SOLDR_BROKER_FIRST_RESPONSE_MS")["source"]
        .as_str()
        .is_some_and(|source| source.contains("ignored")));
    assert_eq!(row("SOLDR_BROKER_PROGRESS_SILENCE_MS")["effective_ms"], 23);
    assert_eq!(
        row("SOLDR_ROUTE_ACQUIRE_CEILING_MS")["effective_ms"],
        120_000
    );
    assert_eq!(json["broker_endpoint"]["resolution_error"], Value::Null);
    assert!(json["broker_endpoint"]["executable_path"]
        .as_str()
        .is_some_and(|value| value.contains(".soldr")));
    assert!(json["broker_endpoint"]["logical_socket_path"]
        .as_str()
        .is_some_and(|value| value.contains("soldr-broker.sock")));
    assert!(json["broker_endpoint"]["bind_endpoint"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));

    let _ = Command::new(common::soldr_bin())
        .args(["broker", "stop"])
        .env("HOME", &workspace)
        .env("USERPROFILE", &workspace)
        .output();
}

#[test]
fn doctor_reports_missing_target() {
    let workspace = unique_temp_dir("doctor-missing-target");
    seed_rust_toolchain_toml(
        &workspace,
        "[toolchain]\n\
         channel = \"1.94.1\"\n\
         targets = [\"x86_64-unknown-linux-musl\"]\n",
    );
    let log_path = workspace.join("rustup.log");
    let rustup = install_doctor_fake_rustup(
        &log_path,
        &DoctorFakeRustupBehavior {
            toolchain_list: vec!["1.94.1-x86_64-unknown-linux-gnu (default)".to_string()],
            components_installed: Vec::new(),
            // Only the host triple is installed — declared musl target
            // is missing.
            targets_installed: vec!["x86_64-unknown-linux-gnu".to_string()],
        },
    );

    // Isolate SOLDR_CACHE_DIR / HOME / USERPROFILE so the doctor's
    // zccache-bundle probe (which inspects `~/.soldr/bin/zccache-pinned/`)
    // can't trip on a stale pinned install in the host's real
    // `~/.soldr/`. Without this, a stale pinned-zccache `source.json`
    // on the dev machine causes `soldr doctor` to exit 1 with empty
    // stdout — a real bug worth fixing in `resolve_pinned_zccache`
    // separately, but the test should be hermetic regardless.
    let output = Command::new(common::soldr_bin())
        .args(["doctor", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_CACHE_DIR", &workspace)
        .env("HOME", &workspace)
        .env("USERPROFILE", &workspace)
        .output()
        .expect("failed to run soldr doctor --json");

    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor must exit 1 when target drift detected\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("doctor --json must produce JSON");
    assert_eq!(json["drift"], true);
    assert_eq!(
        json["missing_targets"],
        serde_json::json!(["x86_64-unknown-linux-musl"])
    );
    assert_eq!(json["missing_components"], serde_json::json!([]));
}
