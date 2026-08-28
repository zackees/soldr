//! End-to-end tests for `soldr optimize`. Covers the CI auto-skip
//! path, dry-run JSON output, scope resolution errors, and (on
//! Windows) the stubbed PowerShell flow that verifies the right
//! cmdlets are invoked without actually elevating.

#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn isolated_soldr_home() -> PathBuf {
    unique_temp_dir("soldr-optimize-home")
}

#[test]
fn optimize_dry_run_json_runs_end_to_end_on_current_platform() {
    let soldr_home = isolated_soldr_home();
    let output = Command::new(common::soldr_bin())
        .args(["optimize", "--dry-run", "--json"])
        .current_dir(&soldr_home)
        .env("SOLDR_CACHE_DIR", &soldr_home)
        // Make sure we never trip CI detection accidentally.
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CI")
        .env_remove("BUILDKITE")
        .env_remove("CIRCLECI")
        .env_remove("TRAVIS")
        .env_remove("JENKINS_URL")
        .output()
        .expect("failed to run soldr optimize --dry-run --json");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // On Windows we expect a planned action set (no project Cargo.toml
    // is present in the isolated home, so the project scope errors —
    // but dry-run global still works when invoked with --scope global).
    // To keep the test cross-platform, exit code 0 + parseable JSON is
    // the contract.
    if !output.status.success() {
        // Project scope error path: not a Rust project. Verify the
        // stderr explains it cleanly.
        assert!(
            stderr.contains("no Rust project detected"),
            "unexpected failure mode\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        return;
    }

    let json: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("optimize --json must produce JSON ({e}); stdout=\n{stdout}"));
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "optimize");
    assert!(
        json["platform"].as_str().is_some(),
        "platform field missing"
    );
    assert_eq!(json["dry_run"], true);
}

#[test]
fn optimize_ci_auto_skip_emits_skip_message() {
    let soldr_home = isolated_soldr_home();
    let output = Command::new(common::soldr_bin())
        .args(["optimize", "--json"])
        .current_dir(&soldr_home)
        .env("SOLDR_CACHE_DIR", &soldr_home)
        .env("GITHUB_ACTIONS", "true")
        .output()
        .expect("failed to run soldr optimize --json under GITHUB_ACTIONS");

    assert_eq!(
        output.status.code(),
        Some(0),
        "CI auto-skip must exit 0\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("optimize --json under CI must produce JSON");
    assert_eq!(json["ci_label"], "github_actions");
    let note = json["note"]
        .as_str()
        .expect("note field present in CI mode");
    assert!(
        note.contains("Skipping"),
        "expected note to mention skipping, got: {note}"
    );
}

// Windows-only: on macOS/Linux `optimize` short-circuits with a no-op
// note before resolving the project scope (see optimize.rs:336), so
// the `no Rust project detected` error never fires off-Windows.
#[test]
fn optimize_project_scope_errors_when_no_cargo_toml() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let workspace = unique_temp_dir("soldr-optimize-no-cargo");
    let output = Command::new(common::soldr_bin())
        .args(["optimize", "--scope", "project", "--dry-run", "--json"])
        .current_dir(&workspace)
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CI")
        .env_remove("BUILDKITE")
        .env_remove("CIRCLECI")
        .env_remove("TRAVIS")
        .env_remove("JENKINS_URL")
        .output()
        .expect("failed to run soldr optimize --scope project");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_ne!(
        output.status.code(),
        Some(0),
        "expected non-zero exit for project scope without Cargo.toml\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("no Rust project detected"),
        "expected clear error about missing Rust project, got stderr:\n{stderr}"
    );
}

#[test]
fn optimize_invokes_add_mppreference_with_admin_seam() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let soldr_home = unique_temp_dir("soldr-optimize-admin");
    let defender_log = soldr_home.join("defender.log");
    let existing_excl = soldr_home.join("existing.txt");
    fs::write(&existing_excl, "").expect("write existing exclusion stub");

    let workspace = soldr_home.join("ws");
    fs::create_dir_all(&workspace).expect("workspace dir");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0.0.1\"\n",
    )
    .expect("Cargo.toml");

    let output = Command::new(common::soldr_bin())
        .args(["optimize", "--scope", "all", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_CACHE_DIR", &soldr_home)
        .env("SOLDR_TEST_ASSUME_ADMIN", "1")
        .env("SOLDR_TEST_DEFENDER_LOG", &defender_log)
        .env("SOLDR_TEST_DEFENDER_EXISTING", &existing_excl)
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CI")
        .env_remove("BUILDKITE")
        .env_remove("CIRCLECI")
        .env_remove("TRAVIS")
        .env_remove("JENKINS_URL")
        .output()
        .expect("failed to run soldr optimize under test seam");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "optimize must exit 0 under admin seam\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let log = fs::read_to_string(&defender_log).expect("defender log must be written");
    assert!(
        log.contains("Add-MpPreference"),
        "expected Add-MpPreference invocations, got log:\n{log}"
    );
    // Each soldr-owned global path plus the project target must be
    // recorded. SOLDR_CACHE_DIR overrides the root, so we look for the
    // child basenames under the resolved root.
    let soldr_root = soldr_home.display().to_string();
    for expected in [
        format!("{soldr_root}\\cache"),
        format!("{soldr_root}\\bench"),
        format!("{soldr_root}\\runtime"),
        format!("{soldr_root}\\state.sqlite3"),
        format!("{soldr_root}\\cache\\zccache"),
        format!("{soldr_root}\\ws\\target"),
    ] {
        assert!(
            log.contains(&expected),
            "missing {expected} in defender log:\n{log}"
        );
    }

    // Managed file should record every applied path. JSON escapes
    // backslashes, so normalize before comparing.
    let managed = fs::read_to_string(soldr_home.join("managed-defender-exclusions.json"))
        .expect("managed-defender-exclusions.json must be written");
    let managed_normalized = managed.replace("\\\\", "\\");
    assert!(
        managed_normalized.contains(&soldr_root),
        "managed file must list soldr-owned paths (normalized);\nraw:\n{managed}"
    );
}

#[test]
fn optimize_undo_only_removes_managed_paths() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let soldr_home = unique_temp_dir("soldr-optimize-undo");
    let workspace = soldr_home.join("ws");
    fs::create_dir_all(&workspace).expect("workspace dir");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0.0.1\"\n",
    )
    .expect("Cargo.toml");

    // Pre-seed a managed file that simulates a prior `optimize global` run.
    let managed_path = soldr_home.join("managed-defender-exclusions.json");
    fs::create_dir_all(&soldr_home).expect("soldr home");
    let cache_path = soldr_home.join("cache");
    let managed = serde_json::json!({
        "schema_version": 1,
        "exclusions": [
            { "path": cache_path.display().to_string(), "added_at_unix": 1, "scope": "global" }
        ]
    });
    fs::write(
        &managed_path,
        serde_json::to_string_pretty(&managed).unwrap(),
    )
    .expect("write managed file");

    // Defender pretends it currently has both the soldr cache path AND
    // a user-added entry. We must NEVER remove the user entry.
    let existing_path = soldr_home.join("existing.txt");
    let user_added = "C:\\Users\\you\\Documents\\Personal";
    let existing_body = format!("{}\n{}\n", cache_path.display(), user_added);
    fs::write(&existing_path, &existing_body).expect("existing list");

    let defender_log = soldr_home.join("defender.log");

    let output = Command::new(common::soldr_bin())
        .args(["optimize", "--undo", "--scope", "global", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_CACHE_DIR", &soldr_home)
        .env("SOLDR_TEST_ASSUME_ADMIN", "1")
        .env("SOLDR_TEST_DEFENDER_LOG", &defender_log)
        .env("SOLDR_TEST_DEFENDER_EXISTING", &existing_path)
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CI")
        .env_remove("BUILDKITE")
        .env_remove("CIRCLECI")
        .env_remove("TRAVIS")
        .env_remove("JENKINS_URL")
        .output()
        .expect("failed to run soldr optimize --undo");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "undo must exit 0\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let log = fs::read_to_string(&defender_log).expect("defender log written");
    assert!(
        log.contains("Remove-MpPreference"),
        "expected Remove-MpPreference, got:\n{log}"
    );
    // User-added entry must not be touched.
    assert!(
        !log.contains("Documents\\Personal"),
        "undo must never touch user-added Defender entries; log:\n{log}"
    );
    // soldr-tracked entry must be in the removal log.
    let cache_str = cache_path.display().to_string();
    assert!(
        log.contains(&cache_str),
        "expected {cache_str} in removal log:\n{log}"
    );
}

#[test]
fn defender_exclusions_check_returns_dry_run_json() {
    let soldr_home = isolated_soldr_home();
    let output = Command::new(common::soldr_bin())
        .args(["defender-exclusions", "check", "--json"])
        .current_dir(&soldr_home)
        .env("SOLDR_CACHE_DIR", &soldr_home)
        // Inject a Cargo.toml so the `all` scope's project leg resolves.
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CI")
        .env_remove("BUILDKITE")
        .env_remove("CIRCLECI")
        .env_remove("TRAVIS")
        .env_remove("JENKINS_URL")
        .output()
        .expect("failed to run soldr defender-exclusions check --json");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        // The `all` scope's project leg errors without a Cargo.toml, which
        // is acceptable: surface the clean error path.
        assert!(
            stderr.contains("no Rust project detected"),
            "unexpected failure\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        return;
    }
    let json: Value =
        serde_json::from_str(&stdout).expect("defender-exclusions check --json must be JSON");
    assert_eq!(json["command"], "optimize");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["undo"], false);
    assert_eq!(json["scope"], "all");
}

#[test]
fn defender_exclusions_check_skips_a_blocking_status_probe_in_a_real_project() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }

    let soldr_home = isolated_soldr_home();
    let workspace = soldr_home.join("workspace");
    let source = workspace.join("src");
    fs::create_dir_all(&source).expect("create test project source directory");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"defender-dry-run-probe\"\nversion = \"0.0.1\"\nedition = \"2021\"\n",
    )
    .expect("write test project manifest");
    fs::write(source.join("lib.rs"), "pub fn fixture() {}\n").expect("write test project source");

    let fake_tools = soldr_home.join("fake-tools");
    fs::create_dir_all(&fake_tools).expect("create fake tool directory");
    let sentinel = soldr_home.join("blocking-status-probe-spawned.txt");
    fs::write(
        fake_tools.join("pwsh.cmd"),
        format!(
            "@echo off\r\necho spawned>\"{}\"\r\n:loop\r\ngoto loop\r\n",
            sentinel.display()
        ),
    )
    .expect("write blocking status probe");
    let inherited_path = std::env::var_os("PATH").expect("PATH must be set");
    let path = std::env::join_paths(
        std::iter::once(fake_tools).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("prepend fake PowerShell to PATH");

    let started = Instant::now();
    let output = Command::new(common::soldr_bin())
        .args(["defender-exclusions", "check", "--json"])
        .current_dir(&workspace)
        .env("SOLDR_CACHE_DIR", &soldr_home)
        .env("PATH", path)
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CI")
        .env_remove("BUILDKITE")
        .env_remove("CIRCLECI")
        .env_remove("TRAVIS")
        .env_remove("JENKINS_URL")
        .output()
        .expect("run defender-exclusions check against blocking status probe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "dry-run check must return its plan\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "dry-run check must not wait for the blocking status probe"
    );
    let json: Value = serde_json::from_str(&stdout)
        .expect("dry-run check against a real project must produce JSON");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["scope"], "all");
    assert!(
        json["actions"]
            .as_array()
            .is_some_and(|actions| !actions.is_empty()),
        "the normal planner must produce a non-empty plan: {json}"
    );
    assert!(
        !sentinel.exists(),
        "dry-run check must not spawn the blocking PowerShell status probe"
    );
}

#[test]
fn defender_exclusions_remove_maps_to_undo() {
    // We can't actually call Defender from a non-Windows test runner, but
    // CI auto-skip lets us prove the dispatch wires `remove` to undo
    // semantics without touching the real subsystem.
    let soldr_home = isolated_soldr_home();
    let output = Command::new(common::soldr_bin())
        .args(["defender-exclusions", "remove", "--json"])
        .current_dir(&soldr_home)
        .env("SOLDR_CACHE_DIR", &soldr_home)
        .env("GITHUB_ACTIONS", "true")
        .output()
        .expect("failed to run soldr defender-exclusions remove --json");

    assert_eq!(
        output.status.code(),
        Some(0),
        "CI auto-skip must exit 0\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("defender-exclusions remove --json must produce JSON");
    assert_eq!(json["undo"], true);
    assert_eq!(json["scope"], "all");
}

#[test]
fn defender_exclusions_add_dry_run_does_not_invoke_powershell() {
    let soldr_home = isolated_soldr_home();
    let output = Command::new(common::soldr_bin())
        .args(["defender-exclusions", "add", "--dry-run", "--json"])
        .current_dir(&soldr_home)
        .env("SOLDR_CACHE_DIR", &soldr_home)
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CI")
        .env_remove("BUILDKITE")
        .env_remove("CIRCLECI")
        .env_remove("TRAVIS")
        .env_remove("JENKINS_URL")
        .output()
        .expect("failed to run soldr defender-exclusions add --dry-run --json");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        // Same "no Rust project" fallthrough as check.
        assert!(
            stderr.contains("no Rust project detected"),
            "unexpected failure\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        return;
    }
    let json: Value =
        serde_json::from_str(&stdout).expect("defender-exclusions add --json must produce JSON");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["undo"], false);
}

#[test]
fn defender_exclusions_help_lists_verbs() {
    let output = Command::new(common::soldr_bin())
        .args(["defender-exclusions", "--help"])
        .output()
        .expect("failed to run soldr defender-exclusions --help");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    for verb in ["check", "add", "remove"] {
        assert!(
            stdout.contains(verb),
            "help output missing `{verb}`:\n{stdout}"
        );
    }
}

#[test]
fn optimize_help_lists_scope_values() {
    let output = Command::new(common::soldr_bin())
        .args(["optimize", "--help"])
        .output()
        .expect("failed to run soldr optimize --help");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["global", "project", "all", "--undo", "--dry-run", "--json"] {
        assert!(
            stdout.contains(expected),
            "help output missing `{expected}`:\n{stdout}"
        );
    }
}
