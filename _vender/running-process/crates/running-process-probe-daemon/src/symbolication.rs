//! Spawning and supervising the symbolization worker (#637).
//!
//! The daemon never parses a symbol file. It hands a capture to a short-lived
//! child, reads a report back, and treats anything else as a degraded
//! symbolization. That the child is a separate *process* is the whole point:
//! a PDB or minidump can be malformed in ways that crash a parser outright
//! rather than returning an error, and a crash cannot be caught in-process.
//!
//! # Every failure is degraded, never fatal
//!
//! A missing worker binary, a crash, a timeout, unreadable output — all
//! produce a [`WorkerError`] the caller reports alongside the raw capture.
//! The daemon is long-lived and shared; losing it because one capture was
//! unsymbolizable would take every other registrant's diagnostics with it.
//!
//! # Deadline
//!
//! The child is bounded by wall-clock time and killed if it overruns. A
//! symbolization worker that hangs — waiting on a network symbol server, or
//! looping on a crafted input — must not pin a daemon thread indefinitely.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use running_process::spawn::{SpawnStdio, StdioSource};

/// Environment variable naming an explicit worker binary.
///
/// Tests point this at a build artifact; deployments rely on the sibling
/// lookup, since the daemon and worker ship together.
pub const WORKER_PATH_ENV: &str = "RUNNING_PROCESS_PROBE_WORKER";

/// How long a worker may run before it is killed.
pub const DEFAULT_WORKER_TIMEOUT: Duration = Duration::from_secs(60);

/// Largest report accepted back from a worker.
///
/// The worker is ours, but it parses hostile input, so its output is treated
/// as untrusted too — a compromised or confused worker must not be able to
/// exhaust the daemon's memory through its stdout.
pub const MAX_REPORT_BYTES: u64 = 64 * 1024 * 1024;

/// Why symbolization did not produce a report.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// The worker binary could not be located.
    #[error("symbolization worker not found (set {WORKER_PATH_ENV} to override)")]
    NotFound,
    /// The worker could not be started.
    #[error("cannot start symbolization worker: {0}")]
    Spawn(#[source] std::io::Error),
    /// Talking to the worker failed.
    #[error("symbolization worker I/O failed: {0}")]
    Io(#[source] std::io::Error),
    /// The worker exited non-zero — including by crashing, which is the
    /// isolation contract working as designed.
    #[error("symbolization worker exited with code {code}: {stderr}")]
    WorkerDied {
        /// Exit status reported by the OS.
        code: i32,
        /// Whatever the worker managed to say before dying.
        stderr: String,
    },
    /// The worker outlived its deadline and was killed.
    #[error("symbolization worker exceeded its {0:?} deadline and was killed")]
    Timeout(Duration),
    /// The worker's output was not a report.
    #[error("symbolization worker produced unreadable output: {0}")]
    BadReport(String),
}

/// Locate the worker binary.
///
/// The override wins; otherwise it is looked for beside the running
/// executable, because the daemon and worker are built and shipped together.
pub fn worker_path() -> Option<PathBuf> {
    resolve_worker_path(std::env::var_os(WORKER_PATH_ENV))
}

/// Resolution logic for [`worker_path`], with the override supplied directly.
///
/// Taking the override as an argument keeps the decision testable without
/// mutating process-global environment state, which would otherwise make the
/// tests order-dependent on each other.
pub fn resolve_worker_path(explicit: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(explicit) = explicit {
        let path = PathBuf::from(explicit);
        // An override naming a missing file resolves to nothing rather than
        // falling back: silently symbolizing with a different binary than the
        // operator named would make the override untrustworthy.
        return path.is_file().then_some(path);
    }
    let exe = std::env::current_exe().ok()?;
    let sibling = exe.parent()?.join(format!(
        "running-process-probe-worker{}",
        std::env::consts::EXE_SUFFIX
    ));
    sibling.is_file().then_some(sibling)
}

/// Hand `capture_json` to a worker and read back its report.
///
/// `capture_json` is passed through opaquely: the daemon deliberately does not
/// model the capture schema beyond what it needs to route it, so a schema
/// change does not require a daemon release.
pub fn symbolize_with_worker(
    capture_json: &[u8],
    timeout: Duration,
) -> Result<String, WorkerError> {
    let binary = worker_path().ok_or(WorkerError::NotFound)?;
    symbolize_with_worker_at_args(&binary, &[], capture_json, timeout)
}

/// Hand a capture to the worker's human-readable renderer.
pub fn symbolize_with_worker_text(
    capture_json: &[u8],
    timeout: Duration,
) -> Result<String, WorkerError> {
    let binary = worker_path().ok_or(WorkerError::NotFound)?;
    symbolize_with_worker_at_args(&binary, &["--text"], capture_json, timeout)
}

/// Like [`symbolize_with_worker`] but against an explicit binary.
pub fn symbolize_with_worker_at(
    binary: &Path,
    capture_json: &[u8],
    timeout: Duration,
) -> Result<String, WorkerError> {
    symbolize_with_worker_at_args(binary, &[], capture_json, timeout)
}

/// Like [`symbolize_with_worker_text`] but against an explicit binary.
pub fn symbolize_with_worker_at_text(
    binary: &Path,
    capture_json: &[u8],
    timeout: Duration,
) -> Result<String, WorkerError> {
    symbolize_with_worker_at_args(binary, &["--text"], capture_json, timeout)
}

fn symbolize_with_worker_at_args(
    binary: &Path,
    args: &[&str],
    capture_json: &[u8],
    timeout: Duration,
) -> Result<String, WorkerError> {
    // Check the binary exists before spawning. On Unix a missing program is
    // NOT reported as a spawn error here: exec fails in the forked child, and
    // the child cannot write its errno back through std's report pipe, so it
    // aborts (SIGABRT) and the parent sees a dead worker instead of `Err`.
    // Without this check a typo in the override surfaces as "worker exited
    // with code -6" rather than "worker not found".
    if !binary.is_file() {
        return Err(WorkerError::NotFound);
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);

    // Routed through the sanitized spawn layer so the child gets sanitized
    // handles and no visible console, like every other spawn in the workspace.
    let mut command = std::process::Command::new(binary);
    command.args(args);
    let mut child = running_process::spawn::spawn(
        &mut command,
        SpawnStdio {
            stdin: StdioSource::Pipe,
            stdout: StdioSource::Pipe,
            stderr: StdioSource::Pipe,
            ..Default::default()
        },
    )
    .map_err(WorkerError::Spawn)?;

    // Write the capture and close stdin. Closing is what tells the worker the
    // capture is complete; without it both sides wait for the other.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| WorkerError::Io(std::io::Error::other("worker stdin was not piped")))?;
    // A worker that dies before reading breaks the pipe. That is not an
    // I/O bug on our side — the exit status below is the real diagnosis,
    // so record the write failure and keep going.
    let input = capture_json.to_vec();
    let stdin_writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        let _ = stdin.flush();
    });

    // Drain stdout and stderr on threads. Reading them in sequence would
    // deadlock as soon as the worker filled the pipe we were not reading.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(handle) = stdout.as_mut() {
            let _ = handle.take(MAX_REPORT_BYTES).read_to_end(&mut buffer);
        }
        buffer
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(handle) = stderr.as_mut() {
            // Bounded too: stderr is diagnostic text, and a worker looping on
            // an error message should not grow the daemon's memory.
            let _ = handle.take(MAX_REPORT_BYTES).read_to_end(&mut buffer);
        }
        buffer
    });

    let code = loop {
        match child.try_wait() {
            Ok(Some(code)) => break code,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdin_writer.join();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(WorkerError::Io(error));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            // Reap so the killed child does not linger as a zombie.
            let _ = child.wait();
            let _ = stdin_writer.join();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(WorkerError::Timeout(timeout));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let _ = stdin_writer.join();
    let stdout_bytes = stdout_reader.join().unwrap_or_default();
    let stderr_text = String::from_utf8_lossy(&stderr_reader.join().unwrap_or_default())
        .trim()
        .to_string();

    if code != 0 {
        return Err(WorkerError::WorkerDied {
            code,
            stderr: stderr_text,
        });
    }

    let report = String::from_utf8(stdout_bytes)
        .map_err(|e| WorkerError::BadReport(format!("report was not UTF-8: {e}")))?;
    if report.trim().is_empty() {
        return Err(WorkerError::BadReport(
            "worker exited successfully but wrote no report".into(),
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURE: &str = r#"{"format":"cooperative_frames","modules":[{"name":"m.dll"}],
        "threads":[{"os_tid":7,"frames":[{"module_index":0,"relative_address":16}]}]}"#;

    /// Path to the worker binary built alongside these tests.
    ///
    /// Returns `None` on a targeted local run that built only this crate. A
    /// workspace run — which is what CI and `./test` do — always produces it,
    /// so a miss *there* means the tests below would skip silently and prove
    /// nothing. That is failed loudly rather than tolerated.
    fn worker_binary() -> Option<PathBuf> {
        let mut path = std::env::current_exe().ok()?;
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        let candidate = path.join(format!(
            "running-process-probe-worker{}",
            std::env::consts::EXE_SUFFIX
        ));
        if candidate.is_file() {
            return Some(candidate);
        }
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "worker binary missing at {} during a CI run; these tests would \
             skip and assert nothing",
            candidate.display()
        );
        None
    }

    #[test]
    fn a_capture_round_trips_through_a_real_worker() {
        let Some(binary) = worker_binary() else {
            // The worker is a separate crate; a filtered build may not have
            // produced it. Skipping is honest — asserting nothing would not be.
            eprintln!("skipping: worker binary not built");
            return;
        };

        let report = symbolize_with_worker_at(&binary, CAPTURE.as_bytes(), DEFAULT_WORKER_TIMEOUT)
            .expect("worker should symbolize a well-formed capture");
        assert!(
            report.contains("m.dll"),
            "report should name the module; got {report}"
        );
    }

    /// The isolation contract, observed from the daemon's side.
    #[test]
    fn a_worker_that_rejects_its_input_is_reported_not_propagated() {
        let Some(binary) = worker_binary() else {
            eprintln!("skipping: worker binary not built");
            return;
        };

        let error = symbolize_with_worker_at(&binary, &[0xFF; 4096], DEFAULT_WORKER_TIMEOUT)
            .expect_err("garbage must not produce a report");
        match error {
            WorkerError::WorkerDied { code, stderr } => {
                assert_ne!(code, 0);
                assert!(!stderr.is_empty(), "the worker should say why it failed");
            }
            other => panic!("expected WorkerDied, got {other}"),
        }

        // The daemon is unaffected and can symbolize immediately afterwards.
        let report = symbolize_with_worker_at(&binary, CAPTURE.as_bytes(), DEFAULT_WORKER_TIMEOUT)
            .expect("a prior failure must not affect later work");
        assert!(report.contains("m.dll"));
    }

    /// A missing binary must be reported as such on every platform.
    ///
    /// Discovered in CI: on Unix the sanitized spawn layer does *not* return
    /// `Err` for a nonexistent program. exec fails in the forked child, which
    /// then cannot write its errno back through std's report pipe and aborts,
    /// so the parent observed `WorkerDied { code: -6 }` — an unrecognizable
    /// diagnosis for "you named a file that isn't there". The existence check
    /// makes the answer the same everywhere.
    #[test]
    fn a_missing_binary_is_reported_as_not_found() {
        let missing = PathBuf::from("definitely-not-a-real-worker-binary");
        let error = symbolize_with_worker_at(&missing, CAPTURE.as_bytes(), DEFAULT_WORKER_TIMEOUT)
            .expect_err("a missing binary cannot symbolize");
        assert!(
            matches!(error, WorkerError::NotFound),
            "expected NotFound, got {error}"
        );
    }

    #[test]
    fn the_override_wins_when_it_names_a_file() {
        let Some(binary) = worker_binary() else {
            eprintln!("skipping: worker binary not built");
            return;
        };
        let resolved = resolve_worker_path(Some(binary.clone().into_os_string()));
        assert_eq!(resolved.as_deref(), Some(binary.as_path()));
    }

    /// An override naming a missing file must resolve to nothing, not fall
    /// back — silently using a different binary than the operator named would
    /// make the override untrustworthy.
    #[test]
    fn an_override_naming_a_nonexistent_file_resolves_to_nothing() {
        let resolved = resolve_worker_path(Some("no-such-worker-binary-anywhere".into()));
        assert_eq!(resolved, None);
    }

    #[test]
    #[ignore]
    fn worker_timeout_helper() {
        std::thread::sleep(Duration::from_secs(10));
    }

    #[test]
    #[ignore]
    fn worker_crash_helper() {
        std::process::abort();
    }

    /// A parser that wedges is killed at the process boundary and reported;
    /// it cannot pin the daemon thread indefinitely.
    #[test]
    fn a_hung_worker_is_killed_at_its_deadline() {
        let current_test = std::env::current_exe().expect("test executable");
        let started = Instant::now();
        let capture = vec![b'x'; 8 * 1024 * 1024];
        let error = symbolize_with_worker_at_args(
            &current_test,
            &[
                "--exact",
                "symbolication::tests::worker_timeout_helper",
                "--ignored",
                "--nocapture",
            ],
            &capture,
            Duration::from_millis(100),
        )
        .expect_err("the helper must exceed the deadline");
        assert!(matches!(error, WorkerError::Timeout(_)), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "deadline enforcement took {:?}",
            started.elapsed()
        );
    }

    /// A hard process abort is contained exactly like a malformed native
    /// parser: only the disposable child dies.
    #[test]
    fn an_aborting_worker_is_contained() {
        let current_test = std::env::current_exe().expect("test executable");
        let error = symbolize_with_worker_at_args(
            &current_test,
            &[
                "--exact",
                "symbolication::tests::worker_crash_helper",
                "--ignored",
                "--nocapture",
            ],
            CAPTURE.as_bytes(),
            DEFAULT_WORKER_TIMEOUT,
        )
        .expect_err("the helper aborts");
        assert!(matches!(error, WorkerError::WorkerDied { .. }), "{error}");

        if let Some(binary) = worker_binary() {
            let report =
                symbolize_with_worker_at(&binary, CAPTURE.as_bytes(), DEFAULT_WORKER_TIMEOUT)
                    .expect("daemon must remain usable after a worker crash");
            assert!(report.contains("m.dll"));
        }
    }
}
