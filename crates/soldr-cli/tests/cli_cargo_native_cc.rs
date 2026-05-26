//! Integration tests for the native C/C++ env-injection (issue #310).
//!
//! These tests do NOT exercise a real C compile — that would need a
//! working cc + headers on every platform the test runs, which is fragile
//! and orthogonal to what we're verifying. Instead the fixture has a
//! `build.rs` that records the `CC` / `CXX` / `CC_KNOWN_WRAPPER_CUSTOM`
//! env vars (and a couple of target-specific variants) to a marker file,
//! and the test asserts soldr injected the wrapper.
//!
//! What this proves end-to-end:
//!   1. `cargo_front_door::run_cargo_front_door` calls
//!      `native_cc::inject_native_cache_env` on the cargo subprocess.
//!   2. The injected `CC` / `CXX` values start with the resolved zccache
//!      binary path.
//!   3. `CC_KNOWN_WRAPPER_CUSTOM=zccache` reaches the cargo child env.
//!   4. The `SOLDR_NATIVE_CACHE=0` opt-out actually suppresses the
//!      injection.
//!   5. A pre-existing `CC=<some wrapper> <compiler>` value is left
//!      untouched (no double-wrap).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn toml_string(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn soldr_bin() -> &'static str {
    env!("CARGO_BIN_EXE_soldr")
}

/// Build a project whose `build.rs` writes the env it sees to a single
/// JSON-shaped file. No actual C compile happens — we just want the env
/// snapshot. Returns the project dir.
fn make_env_capture_project(label: &str) -> PathBuf {
    let dir = unique_temp_dir(label);
    let src = dir.join("src");
    fs::create_dir_all(&src).expect("create src");
    let marker_path = dir.join("env-capture.txt");
    let marker_str = toml_string(&marker_path);

    fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "native_cc_probe"
version = "0.0.1"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    )
    .expect("write Cargo.toml");

    // build.rs writes one `KEY=VALUE` line per env we care about, then
    // a marker line so the test can tell the build actually ran.
    let build_rs = format!(
        r##"use std::fs;
use std::io::Write;

fn main() {{
    // Record every env var we care about. Missing vars get a literal
    // `<UNSET>` so the test can tell apart "soldr didn't set it" from
    // "soldr set it to empty".
    let keys = [
        "CC", "CXX",
        "CC_KNOWN_WRAPPER_CUSTOM",
        "CC_x86_64_unknown_linux_gnu", "CXX_x86_64_unknown_linux_gnu",
        "CC_x86_64_apple_darwin",      "CXX_x86_64_apple_darwin",
        "CC_aarch64_apple_darwin",     "CXX_aarch64_apple_darwin",
        "CC_x86_64_pc_windows_msvc",   "CXX_x86_64_pc_windows_msvc",
        "CC_aarch64_pc_windows_msvc",  "CXX_aarch64_pc_windows_msvc",
    ];
    let mut out = fs::File::create("{marker}").expect("open marker");
    for k in keys.iter() {{
        let v = std::env::var(k).unwrap_or_else(|_| "<UNSET>".to_string());
        writeln!(out, "{{}}={{}}", k, v).unwrap();
    }}
    writeln!(out, "BUILD_RS_DID_RUN=1").unwrap();
}}
"##,
        marker = marker_str
    );
    fs::write(dir.join("build.rs"), build_rs).expect("write build.rs");

    fs::write(src.join("lib.rs"), "pub fn x() -> i32 { 42 }\n").expect("write lib.rs");

    dir
}

fn run_soldr_cargo_build(project: &Path, env_overrides: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(soldr_bin());
    cmd.current_dir(project);
    // Hermetic caches per test run.
    cmd.env("SOLDR_CACHE_DIR", project.join(".soldr-cache"));
    cmd.env_remove("SOLDR_BUILD_CACHE_MODE");
    cmd.env_remove("SOLDR_TARGET_CACHE_MODE");
    for (k, v) in env_overrides {
        cmd.env(k, v);
    }
    cmd.args(["cargo", "build", "--no-trampoline"]);
    cmd.output().expect("spawn soldr cargo build")
}

fn parse_captured_env(marker_path: &Path) -> std::collections::HashMap<String, String> {
    let text = fs::read_to_string(marker_path)
        .unwrap_or_else(|_| panic!("env-capture file missing at {}", marker_path.display()));
    text.lines()
        .filter_map(|line| {
            let (k, v) = line.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

#[test]
fn injects_zccache_wrapped_cc_and_cxx_by_default() {
    let project = make_env_capture_project("native-cc-default");
    let output = run_soldr_cargo_build(&project, &[]);
    assert!(
        output.status.success(),
        "soldr cargo build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let env = parse_captured_env(&project.join("env-capture.txt"));
    assert_eq!(
        env.get("BUILD_RS_DID_RUN").map(String::as_str),
        Some("1"),
        "build.rs marker missing"
    );

    // CC_KNOWN_WRAPPER_CUSTOM must be set to "zccache" so cc-rs strips
    // the wrapper when classifying the real compiler underneath.
    assert_eq!(
        env.get("CC_KNOWN_WRAPPER_CUSTOM").map(String::as_str),
        Some("zccache"),
        "CC_KNOWN_WRAPPER_CUSTOM should be set to 'zccache'; got: {:?}",
        env.get("CC_KNOWN_WRAPPER_CUSTOM")
    );

    // On Unix the default-synth path is on, so CC + CXX are always set.
    // On Windows we only wrap when the user pre-sets them, so this test
    // case expects the InjectExistingOnly behaviour (CC stays <UNSET>).
    let cc = env.get("CC").cloned().unwrap_or_default();
    if cfg!(target_os = "windows") {
        assert_eq!(
            cc, "<UNSET>",
            "Windows default keeps CC unset so cc-rs's vcvars autodetection still runs"
        );
    } else {
        // The injected value is "<zccache-path> cc". Just look for the
        // word "zccache" at the start; the exact path is platform/
        // user-cache dependent.
        let first_token = cc.split_whitespace().next().unwrap_or("");
        let stem = Path::new(first_token)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(first_token);
        assert!(
            stem.eq_ignore_ascii_case("zccache"),
            "CC should be wrapped with zccache; got: {cc:?}"
        );
        let cxx = env.get("CXX").cloned().unwrap_or_default();
        let cxx_first = cxx.split_whitespace().next().unwrap_or("");
        let cxx_stem = Path::new(cxx_first)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(cxx_first);
        assert!(
            cxx_stem.eq_ignore_ascii_case("zccache"),
            "CXX should be wrapped with zccache; got: {cxx:?}"
        );
    }
}

#[test]
fn soldr_native_cache_off_disables_injection() {
    let project = make_env_capture_project("native-cc-disabled");
    let output = run_soldr_cargo_build(&project, &[("SOLDR_NATIVE_CACHE", "0")]);
    assert!(output.status.success(), "soldr cargo build failed");
    let env = parse_captured_env(&project.join("env-capture.txt"));
    // CC stays at whatever the build.rs's inherited env had (likely
    // <UNSET> in this test). The critical assertion is the marker:
    // CC_KNOWN_WRAPPER_CUSTOM must NOT be set to "zccache" when the
    // user opted out — meaning if the test runner didn't set it,
    // the build.rs sees <UNSET>.
    assert_eq!(
        env.get("CC_KNOWN_WRAPPER_CUSTOM").map(String::as_str),
        Some("<UNSET>"),
        "SOLDR_NATIVE_CACHE=0 must suppress CC_KNOWN_WRAPPER_CUSTOM injection"
    );
    // CC must NOT start with zccache.
    let cc = env.get("CC").cloned().unwrap_or_default();
    let first_stem = Path::new(cc.split_whitespace().next().unwrap_or(""))
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    assert!(
        !first_stem.eq_ignore_ascii_case("zccache"),
        "opt-out should leave CC unwrapped; got: {cc:?}"
    );
}

#[test]
fn explicit_user_cc_is_wrapped_on_every_platform() {
    // Issue #310 acceptance criterion: "Existing user compiler
    // selections are preserved and wrapped, not replaced." We pass
    // CC=fake-compiler-doesnt-exist (the build.rs doesn't actually
    // invoke it — we only read the env) and assert soldr wrapped it
    // with zccache. This is the path that lets Windows users opt
    // into native-cache by setting CC explicitly.
    let project = make_env_capture_project("native-cc-explicit");
    let output = run_soldr_cargo_build(&project, &[("CC", "fake-compiler-doesnt-exist")]);
    assert!(output.status.success(), "soldr cargo build failed");
    let env = parse_captured_env(&project.join("env-capture.txt"));

    let cc = env.get("CC").cloned().unwrap_or_default();
    assert!(
        cc.ends_with("fake-compiler-doesnt-exist"),
        "user's compiler should survive at the end of the wrapped CC; got: {cc:?}"
    );
    let first_stem = Path::new(cc.split_whitespace().next().unwrap_or(""))
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    assert!(
        first_stem.eq_ignore_ascii_case("zccache"),
        "CC should be wrapped with zccache when user set it; got: {cc:?}"
    );
}

#[test]
fn pre_wrapped_user_cc_is_not_double_wrapped() {
    // CC="sccache clang" → must remain as-is (no double-wrap).
    let project = make_env_capture_project("native-cc-no-double-wrap");
    let output = run_soldr_cargo_build(&project, &[("CC", "sccache clang")]);
    assert!(output.status.success(), "soldr cargo build failed");
    let env = parse_captured_env(&project.join("env-capture.txt"));

    let cc = env.get("CC").cloned().unwrap_or_default();
    assert_eq!(
        cc, "sccache clang",
        "pre-wrapped sccache CC should be left alone; got: {cc:?}"
    );
}

#[test]
fn no_cache_global_disables_native_too() {
    // `soldr --no-cache cargo …` is the global kill-switch. Native
    // caching must turn off because the zccache session never starts.
    let project = make_env_capture_project("native-cc-no-cache");
    let mut cmd = Command::new(soldr_bin());
    cmd.current_dir(&project);
    cmd.env("SOLDR_CACHE_DIR", project.join(".soldr-cache"));
    cmd.args(["--no-cache", "cargo", "build", "--no-trampoline"]);
    let output = cmd.output().expect("spawn soldr --no-cache cargo build");
    assert!(
        output.status.success(),
        "soldr --no-cache cargo build failed"
    );

    let env = parse_captured_env(&project.join("env-capture.txt"));
    assert_eq!(
        env.get("CC_KNOWN_WRAPPER_CUSTOM").map(String::as_str),
        Some("<UNSET>"),
        "--no-cache must suppress CC_KNOWN_WRAPPER_CUSTOM injection"
    );
    let cc = env.get("CC").cloned().unwrap_or_default();
    let stem = Path::new(cc.split_whitespace().next().unwrap_or(""))
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    assert!(
        !stem.eq_ignore_ascii_case("zccache"),
        "--no-cache should leave CC unwrapped; got: {cc:?}"
    );
}
