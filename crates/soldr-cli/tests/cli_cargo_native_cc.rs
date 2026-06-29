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
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use soldr_cli::timed_test;

mod common;

static SOLDR_CARGO_BUILD_LOCK: Mutex<()> = Mutex::new(());

fn soldr_cargo_build_lock() -> MutexGuard<'static, ()> {
    SOLDR_CARGO_BUILD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn unique_cache_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    // Keep this path short: zccache's Unix daemon socket lives below
    // SOLDR_CACHE_DIR, and macOS has a small sockaddr_un path limit.
    let base = if cfg!(unix) {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!("sdrc-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("failed to create cache dir");
    dir
}

fn toml_string(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn soldr_bin() -> std::path::PathBuf {
    // soldr#1039 phase 1.
    common::soldr_bin()
}

/// Issue #692: the four `run_soldr_cargo_build`-based tests in this
/// file hang on Windows (`STATUS_STACK_BUFFER_OVERRUN` on test-binary
/// teardown) AND on macOS GHA runners (60+ minute open-ended hang in
/// the "Run CLI smoke tests" step). Root cause appears to be
/// daemon-handle lifecycle — the child soldr's spawned `zccache-daemon`
/// / `soldr-daemon` keep stdout/stderr handles open past test exit, so
/// `Command::output()` waits forever for the captured pipes to close.
/// Windows manifests as a hard crash; macOS manifests as an infinite
/// hang that takes out the whole CI matrix.
///
/// Until the daemon-spawn layer is fixed to detach inherited stdio,
/// skip the affected tests on Windows AND macOS. Linux still exercises
/// the full path, so the assertions retain their coverage value.
fn skip_on_windows(test_name: &str) -> bool {
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        let plat = if cfg!(target_os = "windows") {
            "Windows"
        } else {
            "macOS"
        };
        eprintln!(
            "skipping {test_name} on {plat}: daemon-handle lifecycle hang \
             (see #692; restore once the spawned daemon stops inheriting \
             the test runner's stdio handles)"
        );
        true
    } else {
        false
    }
}

struct FakeZccache {
    bin: PathBuf,
    down_marker: PathBuf,
}

fn fake_script_path(dir: &Path, name: &str) -> PathBuf {
    #[cfg(windows)]
    {
        dir.join(format!("{name}.cmd"))
    }
    #[cfg(not(windows))]
    {
        dir.join(name)
    }
}

fn write_fake_script(path: &Path, body: &str) {
    #[cfg(windows)]
    {
        fs::write(path, body.replace('\n', "\r\n")).expect("failed to write fake script");
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, body).expect("failed to write fake script");
        let mut perms = fs::metadata(path)
            .expect("failed to stat fake script")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("failed to chmod fake script");
    }
}

fn fake_zccache_script() -> &'static str {
    #[cfg(windows)]
    {
        r#"@echo off
if "%~1"=="session-start" (
  echo {"session_id":"test-session"}
  exit /b 0
)
if "%~1"=="start" (
  exit /b 0
)
if "%~1"=="session-end" (
  echo {"status":"ok","session_id":"test-session","duration_ms":1,"compilations":0,"hits":0,"misses":0}
  exit /b 0
)
if "%~1"=="stop" (
  if defined SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER type nul > "%SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER%"
  exit /b 0
)
if "%~1"=="status" (
  if defined SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER if exist "%SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER%" (
    echo daemon not running 1>&2
    exit /b 1
  )
  echo hits=0
  exit /b 0
)
set "rustc=%~1"
shift /1
set "args="
:args_loop
if "%~1"=="" goto run_rustc
set args=%args% "%~1"
shift /1
goto args_loop
:run_rustc
call "%rustc%" %args%
exit /b %ERRORLEVEL%
"#
    }
    #[cfg(not(windows))]
    {
        r#"#!/bin/sh
case "$1" in
  start)
    exit 0
    ;;
  session-start)
    printf '{"session_id":"test-session"}\n'
    exit 0
    ;;
  session-end)
    printf '{"status":"ok","session_id":"test-session","duration_ms":1,"compilations":0,"hits":0,"misses":0}\n'
    exit 0
    ;;
  stop)
    if [ -n "${SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER:-}" ]; then
      : > "$SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER"
    fi
    exit 0
    ;;
  status)
    if [ -n "${SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER:-}" ] && [ -e "$SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER" ]; then
      echo 'daemon not running' >&2
      exit 1
    fi
    echo 'hits=0'
    exit 0
    ;;
esac
rustc="$1"
shift
exec "$rustc" "$@"
"#
    }
}

fn install_fake_zccache() -> FakeZccache {
    let dir = unique_temp_dir("native-cc-fake-zccache");
    let zccache = fake_script_path(&dir, "zccache");
    write_fake_script(&zccache, fake_zccache_script());
    FakeZccache {
        bin: zccache,
        down_marker: dir.join("daemon-down"),
    }
}

fn remove_inherited_native_cache_env(cmd: &mut Command) {
    for key in [
        "CC",
        "CXX",
        "CC_KNOWN_WRAPPER_CUSTOM",
        "CC_x86_64_unknown_linux_gnu",
        "CXX_x86_64_unknown_linux_gnu",
        "CC_x86_64_apple_darwin",
        "CXX_x86_64_apple_darwin",
        "CC_aarch64_apple_darwin",
        "CXX_aarch64_apple_darwin",
        "CC_x86_64_pc_windows_msvc",
        "CXX_x86_64_pc_windows_msvc",
        "CC_aarch64_pc_windows_msvc",
        "CXX_aarch64_pc_windows_msvc",
        "CARGO_TARGET_DIR",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "SOLDR_NATIVE_CACHE",
        "SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER",
        "SOLDR_TEST_ZCCACHE_BIN",
        "SOLDR_ZCCACHE_LOCAL_DIR",
        "ZCCACHE_CACHE_DIR",
    ] {
        cmd.env_remove(key);
    }
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
    let _guard = soldr_cargo_build_lock();
    let mut cmd = Command::new(soldr_bin());
    cmd.current_dir(project);
    remove_inherited_native_cache_env(&mut cmd);
    // Hermetic caches per test run, with command-lifetime shutdown so
    // parallel test execution cannot leave multiple zccache daemons alive.
    cmd.env("SOLDR_CACHE_DIR", unique_cache_dir());
    cmd.env("SOLDR_CACHE_LIFECYCLE", "command");
    cmd.env("SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS", "30");
    let fake_zccache = install_fake_zccache();
    cmd.env("SOLDR_TEST_ZCCACHE_BIN", &fake_zccache.bin);
    cmd.env(
        "SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER",
        &fake_zccache.down_marker,
    );
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

fn assert_command_success(output: &std::process::Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

timed_test!(
    injects_zccache_wrapped_cc_and_cxx_by_default,
    Duration::from_secs(120),
    {
        if skip_on_windows("injects_zccache_wrapped_cc_and_cxx_by_default") {
            return;
        }
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
);

timed_test!(
    soldr_native_cache_off_disables_injection,
    Duration::from_secs(120),
    {
        if skip_on_windows("soldr_native_cache_off_disables_injection") {
            return;
        }
        let project = make_env_capture_project("native-cc-disabled");
        let output = run_soldr_cargo_build(&project, &[("SOLDR_NATIVE_CACHE", "0")]);
        assert_command_success(&output, "soldr cargo build");
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
);

timed_test!(
    explicit_user_cc_is_wrapped_on_every_platform,
    Duration::from_secs(120),
    {
        if skip_on_windows("explicit_user_cc_is_wrapped_on_every_platform") {
            return;
        }
        // Issue #310 acceptance criterion: "Existing user compiler
        // selections are preserved and wrapped, not replaced." We pass
        // CC=fake-compiler-doesnt-exist (the build.rs doesn't actually
        // invoke it — we only read the env) and assert soldr wrapped it
        // with zccache. This is the path that lets Windows users opt
        // into native-cache by setting CC explicitly.
        let project = make_env_capture_project("native-cc-explicit");
        let output = run_soldr_cargo_build(&project, &[("CC", "fake-compiler-doesnt-exist")]);
        assert_command_success(&output, "soldr cargo build");
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
);

timed_test!(
    pre_wrapped_user_cc_is_not_double_wrapped,
    Duration::from_secs(120),
    {
        if skip_on_windows("pre_wrapped_user_cc_is_not_double_wrapped") {
            return;
        }
        // CC="sccache clang" → must remain as-is (no double-wrap).
        let project = make_env_capture_project("native-cc-no-double-wrap");
        let output = run_soldr_cargo_build(&project, &[("CC", "sccache clang")]);
        assert_command_success(&output, "soldr cargo build");
        let env = parse_captured_env(&project.join("env-capture.txt"));

        let cc = env.get("CC").cloned().unwrap_or_default();
        assert_eq!(
            cc, "sccache clang",
            "pre-wrapped sccache CC should be left alone; got: {cc:?}"
        );
    }
);

timed_test!(
    no_cache_global_disables_native_too,
    Duration::from_secs(120),
    {
        // `soldr --no-cache cargo …` is the global kill-switch. Native
        // caching must turn off because the zccache session never starts.
        let project = make_env_capture_project("native-cc-no-cache");
        let output = {
            let _guard = soldr_cargo_build_lock();
            let mut cmd = Command::new(soldr_bin());
            cmd.current_dir(&project);
            remove_inherited_native_cache_env(&mut cmd);
            cmd.env("SOLDR_CACHE_DIR", unique_cache_dir());
            cmd.args(["--no-cache", "cargo", "build", "--no-trampoline"]);
            cmd.output().expect("spawn soldr --no-cache cargo build")
        };
        assert_command_success(&output, "soldr --no-cache cargo build");

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
);
