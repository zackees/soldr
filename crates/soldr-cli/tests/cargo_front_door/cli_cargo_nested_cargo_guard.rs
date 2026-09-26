//! soldr#2924: a build-like Cargo launched directly from inside the outer
//! Cargo's lock-holding compile phase — a build script running
//! `Command::new($CARGO) build` against the same target — never re-enters
//! Soldr, so the `IN_SOLDR_PID` entry guard cannot see it, and it waits on the
//! outer Cargo's build-directory lock forever. The Cargo front door observes
//! its Cargo child's process tree and must fail that shape fast instead.
//!
//! These fixtures drive a real toolchain Cargo through the source-built front
//! door: the deadlock is a real Cargo lock, not a simulation. The guard needs
//! the parent-pid edges of the running-process descendant observer, which
//! Linux and macOS report and Windows does not yet (the post-hoc attach
//! observes nothing there), so the fixtures are skipped on Windows.

use crate::common::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const NESTED_CARGO_ENV: &str = "SOLDR_NESTED_CARGO";

fn guard_host_supported() -> bool {
    !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    )
}

/// A two-member workspace whose root package runs `build_rs`. `helper` is a
/// workspace member, so `cargo build -p helper` from the build script (whose
/// working directory is the root manifest directory) targets the very
/// `target/` the outer Cargo is holding locked.
fn nested_workspace(label: &str, build_rs: &str) -> (PathBuf, PathBuf) {
    let root = unique_temp_dir(label);
    let workspace = root.join("ws");
    fs::create_dir_all(workspace.join("src")).expect("workspace src");
    fs::create_dir_all(workspace.join("helper/src")).expect("helper src");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"nested_outer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
         build = \"build.rs\"\n\n[workspace]\nmembers = [\"helper\"]\n",
    )
    .expect("root manifest");
    fs::write(workspace.join("src/lib.rs"), "pub fn outer() {}\n").expect("root lib");
    fs::write(
        workspace.join("helper/Cargo.toml"),
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("helper manifest");
    fs::write(workspace.join("helper/src/lib.rs"), "pub fn helper() {}\n").expect("helper lib");
    // The helper's own build script keeps a nested build of it alive for a
    // couple of seconds, so an isolated nested build is still running when
    // the guard samples the tree instead of racing it to completion.
    fs::write(
        workspace.join("helper/build.rs"),
        "fn main() {\n    println!(\"cargo:rerun-if-changed=build.rs\");\n    \
         std::thread::sleep(std::time::Duration::from_secs(2));\n}\n",
    )
    .expect("helper build.rs");
    fs::write(workspace.join("build.rs"), build_rs).expect("build.rs");
    (root, workspace)
}

/// A build script that stamps the wall-clock instant it launches the nested
/// Cargo, then runs `$CARGO <nested_args>` with `extra_env`, and fails the
/// build if the nested Cargo fails.
fn build_script(marker: &Path, nested_args: &[&str], extra_env: &[(&str, &str)]) -> String {
    let args = nested_args
        .iter()
        .map(|arg| format!("{arg:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let envs = extra_env
        .iter()
        .map(|(key, value)| format!(".env({key:?}, {value})"))
        .collect::<String>();
    format!(
        r#"fn main() {{
    println!("cargo:rerun-if-changed=build.rs");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let _ = &out_dir;
    let permit = std::env::var("{NESTED_CARGO_ENV}").unwrap_or_else(|_| "<unset>".into());
    println!("cargo:warning=nested-permit-env={{permit}}");
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    std::fs::write({marker:?}, started.to_string()).expect("marker");
    let cargo = std::env::var("CARGO").expect("CARGO");
    let status = std::process::Command::new(&cargo)
        .args([{args}])
        {envs}
        .status()
        .expect("spawn nested cargo");
    assert!(status.success(), "nested cargo failed: {{status:?}}");
}}
"#,
        marker = marker.display().to_string(),
    )
}

fn run_outer_build(workspace: &Path, cache_root: &Path, extra_env: &[(&str, &str)]) -> Output {
    let mut command = isolated_soldr_command();
    command
        .args(["--no-cache", "cargo", "build"])
        .current_dir(workspace)
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("ZCCACHE_DISABLE", "1")
        // Backstop only: without the guard the nested Cargo waits forever,
        // and this makes the pre-fix run fail by deadline rather than hang
        // the suite. A guarded run finishes long before it.
        .env("SOLDR_CARGO_WAIT_TIMEOUT_SECS", "45")
        .env_remove(NESTED_CARGO_ENV);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("run soldr cargo build")
}

fn audit_records(cache_root: &Path) -> Vec<serde_json::Value> {
    let dir = cache_root.join("logs").join("nested-cargo");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let text = fs::read_to_string(entry.path()).expect("read audit record");
        records.push(serde_json::from_str(&text).expect("audit record is JSON"));
    }
    records
}

fn unix_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis()
}

fn describe(output: &Output) -> String {
    format!(
        "status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn build_script_nested_cargo_on_the_outer_target_fails_fast() {
    if !guard_host_supported() {
        return;
    }
    let marker_root = unique_temp_dir("nested-cargo-marker");
    let marker = marker_root.join("nested-started-ms");
    let (root, workspace) = nested_workspace(
        "nested-cargo-self-lock",
        &build_script(&marker, &["build", "-p", "helper"], &[]),
    );
    let cache_root = root.join("soldr-cache");

    let output = run_outer_build(&workspace, &cache_root, &[]);
    let exited_ms = unix_ms_now();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "a self-locking nested Cargo must fail the outer run with exit 1\n{}",
        describe(&output)
    );
    assert!(
        stderr.contains("nested Cargo") && stderr.contains("build -p helper"),
        "stderr must carry the bounded nested-Cargo diagnostic\n{}",
        describe(&output)
    );
    assert!(
        !stderr.contains("timed out after"),
        "the guard, not the wall-clock backstop, must end the run\n{}",
        describe(&output)
    );
    let started_ms: u128 = fs::read_to_string(&marker)
        .expect("the build script launched the nested Cargo")
        .trim()
        .parse()
        .expect("marker is a unix-ms stamp");
    let after_start = Duration::from_millis(exited_ms.saturating_sub(started_ms) as u64);
    assert!(
        after_start <= Duration::from_secs(15),
        "the outer run must end promptly after the nested Cargo starts, took {after_start:?}\n{}",
        describe(&output)
    );

    let records = audit_records(&cache_root);
    assert_eq!(records.len(), 1, "one audit record: {records:?}");
    let record = &records[0];
    assert_eq!(record["event"], "nested_cargo_self_lock");
    assert_eq!(record["action"], "terminated");
    assert_eq!(record["verb"], "build");
    let detected_ms = record["unix_ms"].as_u64().expect("unix_ms") as u128;
    assert!(
        detected_ms.saturating_sub(started_ms) <= 5_000,
        "detection must follow the nested start within seconds: record {record}"
    );
    eprintln!(
        "nested-cargo guard timing: detected {}ms and exited {}ms after the nested Cargo started",
        detected_ms.saturating_sub(started_ms),
        after_start.as_millis()
    );
    let nested_pid = record["nested_pid"].as_u64().expect("nested_pid") as u32;
    let deadline = Instant::now() + Duration::from_secs(5);
    while soldr_platform::process::inspect::is_alive(nested_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !soldr_platform::process::inspect::is_alive(nested_pid),
        "the nested Cargo {nested_pid} must not survive the teardown"
    );
}

#[test]
fn non_locking_and_isolated_nested_cargo_are_allowed() {
    if !guard_host_supported() {
        return;
    }
    let marker_root = unique_temp_dir("nested-cargo-allowed-marker");
    let marker = marker_root.join("nested-started-ms");
    let isolated_target = marker_root.join("isolated-target");
    // Three nested commands from one build script: `--version`, a
    // `metadata --no-deps` (neither acquires the build lock), and a build with
    // an explicit absolute `--target-dir` distinct from the outer target.
    let build_rs = format!(
        r#"fn run(args: &[&str]) {{
    let cargo = std::env::var("CARGO").expect("CARGO");
    let status = std::process::Command::new(&cargo)
        .args(args)
        .stdout(std::process::Stdio::null())
        .status()
        .expect("spawn nested cargo");
    assert!(status.success(), "nested cargo {{args:?}} failed: {{status:?}}");
}}

fn main() {{
    println!("cargo:rerun-if-changed=build.rs");
    std::fs::write({marker:?}, "started").expect("marker");
    run(&["--version"]);
    run(&["metadata", "--no-deps", "--format-version", "1"]);
    run(&["build", "-p", "helper", "--target-dir", {target:?}]);
}}
"#,
        marker = marker.display().to_string(),
        target = isolated_target.display().to_string(),
    );
    let (root, workspace) = nested_workspace("nested-cargo-allowed", &build_rs);
    let cache_root = root.join("soldr-cache");

    let output = run_outer_build(&workspace, &cache_root, &[]);
    assert!(
        output.status.success(),
        "proven-safe nested Cargo commands must be allowed\n{}",
        describe(&output)
    );
    assert!(marker.exists(), "the build script ran");
    assert!(
        audit_records(&cache_root).is_empty(),
        "allowed nested Cargo must not be recorded as a hazard"
    );
}

#[test]
fn env_only_target_override_requires_the_scoped_permit() {
    if !guard_host_supported() {
        return;
    }
    let marker_root = unique_temp_dir("nested-cargo-env-marker");
    let marker = marker_root.join("nested-started-ms");
    let isolated_target = marker_root.join("env-target");
    // Isolated in fact — but only through `CARGO_TARGET_DIR`, which the
    // process observer cannot read portably, so it is not guessed as safe.
    let env_value = format!("{:?}", isolated_target.display().to_string());
    let build_rs = build_script(
        &marker,
        &["build", "-p", "helper"],
        &[("CARGO_TARGET_DIR", env_value.as_str())],
    );

    let (root, workspace) = nested_workspace("nested-cargo-env-only", &build_rs);
    let cache_root = root.join("soldr-cache");
    let rejected = run_outer_build(&workspace, &cache_root, &[]);
    assert_eq!(
        rejected.status.code(),
        Some(1),
        "an env-only target override must not be guessed as isolated\n{}",
        describe(&rejected)
    );
    let records = audit_records(&cache_root);
    assert_eq!(records.len(), 1, "one audit record: {records:?}");
    assert_eq!(records[0]["permit"], serde_json::Value::Null);

    let (root, workspace) = nested_workspace("nested-cargo-env-permit", &build_rs);
    let cache_root = root.join("soldr-cache");
    let permitted = run_outer_build(&workspace, &cache_root, &[(NESTED_CARGO_ENV, "allow")]);
    assert!(
        permitted.status.success(),
        "the explicit permit must let the isolated nested build run\n{}",
        describe(&permitted)
    );
    let stderr = String::from_utf8_lossy(&permitted.stderr);
    assert!(
        stderr.contains("nested-permit-env=<unset>"),
        "the permit is scoped to the outer run: Cargo's descendants (and any \
         nested Soldr entry among them) must not inherit it\n{}",
        describe(&permitted)
    );
    assert!(
        stderr.contains(&format!("{NESTED_CARGO_ENV}=allow")),
        "the permit must be named in the diagnostic\n{}",
        describe(&permitted)
    );
    let records = audit_records(&cache_root);
    assert_eq!(
        records.len(),
        1,
        "the permitted nested build is audited: {records:?}"
    );
    assert_eq!(records[0]["permit"], "allow");
    assert_eq!(records[0]["action"], "permitted");
}
