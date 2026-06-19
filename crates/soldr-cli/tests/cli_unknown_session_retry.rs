//! End-to-end integration tests for the zccache `unknown session:`
//! retry path on Windows (issue zackees/soldr#266; depends on #265).
//!
//! When `run_wrapper_through_zccache` on Windows sees the managed
//! zccache wrapper exit with `zccache error: unknown session: <id>`
//! on stderr, soldr should:
//!   1. Allocate a fresh session via `zccache session-start ...`.
//!   2. Re-run the wrapper invocation exactly once, with the new
//!      `ZCCACHE_SESSION_ID` in the retry's env.
//!   3. Stop after one retry — a second `unknown session:` propagates
//!      the failure to cargo.
//!
//! These tests drive the real `soldr` binary as a rustc wrapper, with
//! a tiny stub binary (`fake-zccache-265-stub`) standing in for
//! managed zccache. The stub is gated behind the `test-stubs` cargo
//! feature, so its build is the test's responsibility.
//!
//! The tests are `#[cfg(not(unix))]` because the retry only exists on
//! the Windows path of `run_wrapper_through_zccache`; on Unix the
//! wrapper `exec()`s into zccache and cannot retry.

#![cfg(not(unix))]

// The retry implementation lives on the parent PR (#265); without
// that landed on the same base branch, the test would deadlock waiting
// for soldr to retry. Unignore in the follow-up PR once #265 merges
// to main; a one-line PR removing this `#[ignore]` is the entire
// follow-up.
//
// To run locally before #265 lands, point cargo at the parent branch
// and pass `--ignored`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Build the `fake-zccache-265-stub` binary on demand and return its
/// path. The bin is gated behind `--features test-stubs`, so it is
/// not automatically built by `cargo test`. We invoke cargo via
/// `env!("CARGO")` so we use the toolchain the test runner picked.
fn ensure_fake_zccache_stub() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));

    let soldr_bin = PathBuf::from(env!("CARGO_BIN_EXE_soldr"));
    let bin_dir = soldr_bin
        .parent()
        .expect("CARGO_BIN_EXE_soldr should have a parent dir");

    // When the outer `cargo test` runs with `--target <triple>` (CI on
    // Windows / macOS / Linux), `CARGO_BIN_EXE_soldr` resolves to
    // `target/<triple>/debug/soldr[.exe]`. Without `--target` it is
    // `target/debug/soldr[.exe]`. We need the inner `cargo build` to
    // emit the stub into the *same* directory so the assertion below
    // finds it — otherwise the inner build defaults to `target/debug`
    // and the test fails with "expected stub at .../<triple>/debug/...".
    let target_triple = bin_dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .filter(|name| *name != "target")
        .map(str::to_owned);

    let mut command = Command::new(&cargo);
    command
        .args(["build", "--quiet", "--manifest-path"])
        .arg(format!("{manifest_dir}/Cargo.toml"))
        .args(["--bin", "fake-zccache-265-stub", "--features", "test-stubs"]);
    if let Some(triple) = target_triple.as_deref() {
        command.args(["--target", triple]);
    }
    let output = command
        .output()
        .expect("failed to invoke cargo build for fake-zccache-265-stub");
    assert!(
        output.status.success(),
        "cargo build for stub failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stub_name = if cfg!(windows) {
        "fake-zccache-265-stub.exe"
    } else {
        "fake-zccache-265-stub"
    };
    let stub_path = bin_dir.join(stub_name);
    assert!(
        stub_path.exists(),
        "expected stub at {} after cargo build",
        stub_path.display()
    );
    stub_path
}

fn unique_temp_dir(label: &str) -> TempDir {
    tempfile::Builder::new()
        .prefix(&format!("soldr-266-{label}-"))
        .tempdir()
        .expect("failed to create temp dir")
}

fn read_counter(state_file: &Path, key: &str) -> u64 {
    let Ok(body) = fs::read_to_string(state_file) else {
        return 0;
    };
    for line in body.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                return v.trim().parse::<u64>().unwrap_or(0);
            }
        }
    }
    0
}

/// Common test scaffold: builds the stub, points soldr at it via
/// `SOLDR_TEST_ZCCACHE_BIN`, sets the cache dir, enables cache, and
/// runs `soldr rustc -vV`. `rustc` is a recognised wrapper-stem so
/// `is_wrapper_invocation` returns true and `run_rustc_wrapper` runs.
///
/// Returns the exit code, the state file path so callers can inspect
/// it, and stderr (for debug failure messages). Owns the `TempDir`
/// so its directory is cleaned up when the run is dropped.
struct RetryRun {
    exit_code: i32,
    state_file: PathBuf,
    stderr: String,
    stdout: String,
    // Held to keep the temp dir alive for the duration of the test;
    // dropped at end-of-test to delete everything we created.
    _cache_root: TempDir,
}

fn run_soldr_wrapper(label: &str, mode: Option<&str>) -> RetryRun {
    let stub = ensure_fake_zccache_stub();
    let cache_root = unique_temp_dir(label);
    let cache_root_path = cache_root.path().to_path_buf();
    let zccache_cache_dir = cache_root_path.join("zccache");
    fs::create_dir_all(zccache_cache_dir.join("logs")).expect("failed to seed zccache logs subdir");
    let state_file = cache_root_path.join("stub-state.txt");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_soldr"));
    cmd.arg("rustc").arg("-vV");
    cmd.env("SOLDR_CACHE_ENABLED", "1");
    cmd.env("SOLDR_CACHE_DIR", &cache_root_path);
    cmd.env("SOLDR_TEST_ZCCACHE_BIN", &stub);
    cmd.env("SOLDR_ZCCACHE_SESSION_DIR", &zccache_cache_dir);
    cmd.env("ZCCACHE_CACHE_DIR", &zccache_cache_dir);
    cmd.env("SOLDR_MANAGED_ZCCACHE_CACHE_DIR", &zccache_cache_dir);
    cmd.env("ZCCACHE_SESSION_ID", "initial-session-from-test");
    cmd.env("FAKE_ZCCACHE_STATE_FILE", &state_file);
    if let Some(m) = mode {
        cmd.env("FAKE_ZCCACHE_MODE", m);
    }
    // Block accidental trampoline / version lookup, and don't leak the
    // host PATH (which could contain a real zccache).
    cmd.env_remove("SOLDR_AS");
    cmd.env_remove("RUSTC_WRAPPER");
    cmd.env_remove("SOLDR_RUSTC_WRAPPER");

    let output = cmd
        .output()
        .expect("failed to spawn soldr binary for wrapper test");

    RetryRun {
        exit_code: output.status.code().unwrap_or(-1),
        state_file,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        _cache_root: cache_root,
    }
}

#[test]
fn unknown_session_triggers_one_retry() {
    let run = run_soldr_wrapper("trigger-retry", None);

    assert_eq!(
        run.exit_code, 0,
        "expected soldr to succeed after one retry\nstdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );

    let wrapper_calls = read_counter(&run.state_file, "wrapper_calls");
    let session_starts = read_counter(&run.state_file, "session_starts");
    assert_eq!(
        wrapper_calls, 2,
        "expected exactly two wrapper invocations (initial + retry); got {wrapper_calls}\nstate:\n{}",
        fs::read_to_string(&run.state_file).unwrap_or_default()
    );
    assert!(
        session_starts >= 1,
        "expected at least one session-start invocation between the two wrapper calls; got {session_starts}"
    );
}

#[test]
fn unknown_session_retry_budget_is_one() {
    let run = run_soldr_wrapper("retry-budget", Some("always_fail"));

    assert_ne!(
        run.exit_code, 0,
        "expected soldr to surface the failure after one retry\nstdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );

    let wrapper_calls = read_counter(&run.state_file, "wrapper_calls");
    assert_eq!(
        wrapper_calls, 2,
        "expected exactly two wrapper invocations (initial + one retry, no further attempts); got {wrapper_calls}\nstate:\n{}",
        fs::read_to_string(&run.state_file).unwrap_or_default()
    );
}

#[test]
fn non_unknown_session_failure_does_not_retry() {
    let run = run_soldr_wrapper("no-retry", Some("non_session_failure"));

    assert_ne!(
        run.exit_code, 0,
        "expected soldr to surface the unrelated failure\nstdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );

    let wrapper_calls = read_counter(&run.state_file, "wrapper_calls");
    let session_starts = read_counter(&run.state_file, "session_starts");
    assert_eq!(
        wrapper_calls, 1,
        "expected exactly one wrapper invocation (no retry for unrelated stderr); got {wrapper_calls}\nstate:\n{}",
        fs::read_to_string(&run.state_file).unwrap_or_default()
    );
    assert_eq!(
        session_starts, 0,
        "expected no session-start invocations for an unrelated failure; got {session_starts}"
    );
}
