#![allow(unused_imports)]

mod common;

use common::*;
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

    #[cfg(windows)]
    let rustup = dir.join("rustup.bat");
    #[cfg(not(windows))]
    let rustup = fake_script_path(&dir, "rustup");

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
    #[cfg(windows)]
    {
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
    }
    #[cfg(not(windows))]
    {
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

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["doctor", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
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

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["doctor", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
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

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
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

#[cfg(windows)]
const ZCCACHE_BINARY_EXT: &str = ".exe";
#[cfg(not(windows))]
const ZCCACHE_BINARY_EXT: &str = "";

#[test]
fn doctor_surfaces_local_zccache_override_when_env_var_set() {
    let tmp = unique_temp_dir("doctor-local-zccache");
    let workspace = tmp.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let soldr_cache = tmp.join("soldr-cache");
    fs::create_dir_all(&soldr_cache).unwrap();
    let local_build = tmp.join("local-build");
    fs::create_dir_all(&local_build).unwrap();

    // Plant fake zccache binaries (zero-byte content is fine — soldr
    // copies bytes, doesn't execute them in doctor mode).
    for name in ["zccache", "zccache-daemon", "zccache-fp"] {
        let file = local_build.join(format!("{name}{ZCCACHE_BINARY_EXT}"));
        fs::write(&file, b"fake").unwrap();
    }
    // Plant PDBs for two of three so we can assert the partial count.
    for name in ["zccache", "zccache-daemon"] {
        let pdb = local_build.join(format!("{name}.pdb"));
        fs::write(&pdb, b"pdb").unwrap();
    }

    // No manifest in the workspace → doctor takes the "missing
    // manifest" path but still prints the zccache section.
    let log_path = tmp.join("rustup.log");
    let rustup = install_failing_fake_rustup(&log_path);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["doctor", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_CACHE_DIR", &soldr_cache)
        .env("SOLDR_ZCCACHE_LOCAL_DIR", &local_build)
        .output()
        .expect("failed to run soldr doctor --json");

    assert_eq!(
        output.status.code(),
        Some(0),
        "doctor must exit 0 when manifest is missing\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("doctor --json must produce JSON");
    assert_eq!(json["managed_zccache"]["source"], "local");
    let runtime_dir = json["managed_zccache"]["runtime_dir"]
        .as_str()
        .expect("runtime_dir present");
    assert!(
        runtime_dir.contains("zccache-local-"),
        "runtime_dir should be hash-suffixed: {runtime_dir}"
    );
    // 2/3 PDBs were planted.
    assert_eq!(json["managed_zccache"]["debug_info_found"], 2);
    assert_eq!(json["managed_zccache"]["debug_info_expected"], 3);

    // Doctor's read-only inspection should have surfaced the local
    // build path the user gave us, even before the copy happens.
    let source_dir = json["managed_zccache"]["source_dir"]
        .as_str()
        .expect("source_dir present");
    assert_eq!(
        std::path::Path::new(source_dir)
            .file_name()
            .and_then(|s| s.to_str()),
        Some("local-build")
    );
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

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["doctor", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
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
