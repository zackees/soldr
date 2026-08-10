//! Out-of-process tests for the symbolization worker (#637).
//!
//! These drive the built binary rather than calling `symbolize()` directly.
//! The isolation contract is about what happens to a *process* — that a bad
//! capture ends this PID and nothing else — and an in-process test cannot
//! observe that at all.

use std::io::Write as _;
use std::process::{Command, Stdio};

/// Path to the worker binary built alongside this test.
///
/// Cargo puts integration-test binaries in a subdirectory of the profile dir
/// (`.../debug/deps/`), so the sibling binary is one level up.
fn worker_binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop(); // deps/
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!(
        "running-process-probe-worker{}",
        std::env::consts::EXE_SUFFIX
    ))
}

struct Outcome {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// Run the worker with `input` on stdin.
fn run_worker(input: &[u8]) -> Outcome {
    run_worker_with(input, &[])
}

/// Run the worker with `input` on stdin and extra argv.
#[allow(clippy::disallowed_methods)]
fn run_worker_with(input: &[u8], args: &[&str]) -> Outcome {
    let binary = worker_binary();
    assert!(
        binary.exists(),
        "worker binary missing at {}; the test needs it built",
        binary.display()
    );

    let mut child = Command::new(&binary)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker");

    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("write capture");

    let output = child.wait_with_output().expect("wait for worker");
    Outcome {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

const CAPTURE: &str = r#"{
  "format": "cooperative_frames",
  "modules": [{"name": "fixture.dll"}],
  "threads": [{
    "os_tid": 4242,
    "frames": [{"module_index": 0, "relative_address": 4096}],
    "py_frames": [{"file": "app.py", "line": 3, "func": "handler"}]
  }]
}"#;

#[test]
fn a_valid_capture_produces_a_report_on_stdout() {
    let outcome = run_worker(CAPTURE.as_bytes());
    assert!(
        outcome.status.success(),
        "worker failed: {}",
        outcome.stderr
    );

    let report: serde_json::Value = serde_json::from_str(&outcome.stdout)
        .unwrap_or_else(|e| panic!("stdout was not a JSON report: {e}\n{}", outcome.stdout));

    let thread = &report["threads"][0];
    assert_eq!(thread["os_tid"], 4242);
    assert_eq!(thread["frames"][0]["module"], "fixture.dll");
    assert_eq!(thread["frames"][0]["relative_address"], 4096);
    assert_eq!(thread["frames"][0]["status"], "raw_only");
    // The interpreter half must survive the trip through the worker.
    assert_eq!(thread["py_frames"][0]["func"], "handler");
}

/// The isolation contract: garbage ends the worker, and only the worker.
#[test]
fn a_malformed_capture_exits_non_zero_without_output() {
    let outcome = run_worker(&[0xFF; 4096]);

    assert!(
        !outcome.status.success(),
        "worker accepted 4 KiB of garbage as a capture"
    );
    assert!(
        outcome.stdout.is_empty(),
        "a failed run must emit no report; got {:?}",
        outcome.stdout
    );
    assert!(
        !outcome.stderr.is_empty(),
        "a failed run must say why on stderr"
    );

    // The harness standing in for the daemon is still running, and can still
    // use the worker — the failure was contained to that one process.
    let next = run_worker(CAPTURE.as_bytes());
    assert!(
        next.status.success(),
        "a prior malformed capture must not affect later runs: {}",
        next.stderr
    );
}

/// Well-formed JSON that is not a capture must be refused, not half-accepted.
#[test]
fn json_of_the_wrong_shape_is_refused() {
    let outcome = run_worker(br#"{"threads": "not-a-list"}"#);
    assert!(!outcome.status.success());
    assert!(outcome.stdout.is_empty());
}

/// Empty input is a truncated capture, not an empty one.
#[test]
fn empty_input_is_an_error_not_an_empty_report() {
    let outcome = run_worker(b"");
    assert!(
        !outcome.status.success(),
        "empty stdin must not produce a successful empty report"
    );
}

/// An unsupported format must refuse rather than report zero threads.
#[test]
fn the_minidump_format_is_refused_for_now() {
    let outcome = run_worker(br#"{"format": "minidump", "threads": []}"#);
    assert!(!outcome.status.success());
    assert!(
        outcome.stderr.contains("minidump") || outcome.stderr.contains("Minidump"),
        "stderr should name the unsupported format; got {:?}",
        outcome.stderr
    );
}

/// `--text` must actually reach the binary and change its output.
///
/// The renderer is unit-tested; this asserts the flag is wired, which is the
/// part unit tests cannot see.
#[test]
fn the_text_flag_renders_a_human_readable_report() {
    let outcome = run_worker_with(CAPTURE.as_bytes(), &["--text"]);
    assert!(
        outcome.status.success(),
        "worker failed: {}",
        outcome.stderr
    );

    assert!(
        outcome.stdout.contains("Thread 4242"),
        "expected a rendered thread header, got {:?}",
        outcome.stdout
    );
    assert!(
        outcome.stdout.contains("fixture.dll+0x1000"),
        "expected module+offset, got {:?}",
        outcome.stdout
    );
    // The interpreter half must survive into the text form too.
    assert!(
        outcome.stdout.contains("app.py:3 in handler"),
        "expected the Python frame, got {:?}",
        outcome.stdout
    );
    assert!(
        !outcome.stdout.trim_start().starts_with('{'),
        "--text still emitted JSON: {:?}",
        outcome.stdout
    );
}

/// Without the flag the output stays machine-readable, since the daemon
/// parses it.
#[test]
fn the_default_output_is_still_json() {
    let outcome = run_worker(CAPTURE.as_bytes());
    assert!(
        outcome.status.success(),
        "worker failed: {}",
        outcome.stderr
    );
    serde_json::from_str::<serde_json::Value>(&outcome.stdout).unwrap_or_else(|e| {
        panic!(
            "default output must be JSON: {e}
{}",
            outcome.stdout
        )
    });
}
