//! Per-test watchdog infrastructure.
//!
//! Provides the [`timed_test!`](crate::timed_test) macro and its backing
//! function [`run_with_watchdog`]. Any test wrapped with `timed_test!`
//! runs on a dedicated worker thread; if the body does not return
//! within the configured wall-clock deadline (default 2 minutes) the
//! watchdog dumps its own backtrace to stderr and aborts the entire
//! test binary. That guarantees a hung test can never block the suite.
//!
//! The module is intentionally always-compiled (not behind
//! `cfg(test)`) because cargo compiles the lib without `cfg(test)`
//! when linking it into integration tests; a cfg gate here would hide
//! the module from `tests/`. The runtime cost in production builds is
//! one unused function + one unused constant.
//!
//! ## Why abort instead of unwinding?
//!
//! Rust has no portable way to forcefully kill another thread. Once a
//! test body hangs (e.g. spinning on a never-released lock, blocked on
//! a network read with no timeout, deadlocked in zccache IPC) the only
//! way to free the test runner is to tear the whole process down. We
//! choose `std::process::abort()` so:
//!
//! * the worker thread is unambiguously terminated,
//! * the test harness reports a clear non-zero exit,
//! * CI surfaces the watchdog banner immediately before the abort.
//!
//! ## Limitations
//!
//! `std::backtrace::Backtrace` only captures the watchdog thread's
//! stack — Rust's std does not expose a thread enumerator, so the
//! hung worker's own stack is not printed. Users who need that should
//! attach a debugger to the running test binary before it aborts (or
//! re-run under `cargo test -- --nocapture deliberate_hang` to make
//! the watchdog banner visible).
//!
//! ## Usage
//!
//! ```ignore
//! use soldr_cli::timed_test;
//!
//! timed_test!(my_test, { /* test body */ });
//! timed_test!(slow_test, std::time::Duration::from_secs(300), {
//!     /* test body */
//! });
//! ```

use std::any::Any;
use std::backtrace::Backtrace;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// Default per-test deadline. Picked to comfortably cover the slowest
/// existing test in this crate (the toolchain prepare integration
/// suite) while still surfacing a real hang in a single CI run.
pub const DEFAULT_TEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Stderr banner emitted before [`std::process::abort`] when a test
/// exceeds its watchdog deadline. Tests can grep for this when they
/// spawn the binary as a subprocess (see the self-test feature).
pub const HANG_BANNER_PREFIX: &str = "TEST HUNG";

/// Run `body` under a wall-clock watchdog.
///
/// * Returns normally when the body completes within `timeout`.
/// * Resumes the original panic (preserving the test framework's
///   error rendering) when the body panicked before the deadline.
/// * Emits a diagnostic banner + a watchdog-thread backtrace and
///   calls [`std::process::abort`] when the body is still running
///   after `timeout`.
///
/// Callers normally use the [`timed_test!`](crate::timed_test) macro
/// rather than invoking this directly.
pub fn run_with_watchdog<F>(name: &'static str, timeout: Duration, body: F)
where
    F: FnOnce() + Send + 'static,
{
    type ThreadResult = Result<(), Box<dyn Any + Send + 'static>>;

    let (tx, rx) = channel::<ThreadResult>();
    let worker = thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            // catch_unwind so a panic in `body` is forwarded back to
            // the main test thread and rendered by the standard test
            // harness instead of aborting the worker silently.
            let result = catch_unwind(AssertUnwindSafe(body));
            // If the receiver hung up (e.g. the watchdog already fired
            // and aborted the process) we have nothing to do.
            let _ = tx.send(result);
        })
        .expect("watchdog: failed to spawn worker thread");

    match rx.recv_timeout(timeout) {
        Ok(Ok(())) => {
            // Body returned successfully. Join the worker so any
            // background thread it spawned that the test relied on
            // has a chance to drop cleanly.
            let _ = worker.join();
        }
        Ok(Err(panic_payload)) => {
            // Body panicked. Re-raise so the test harness reports the
            // failure exactly as if the watchdog were absent.
            let _ = worker.join();
            resume_unwind(panic_payload);
        }
        Err(RecvTimeoutError::Timeout) => {
            // Body is still running. Print a banner + a backtrace from
            // *this* (watchdog) thread, then abort the whole binary.
            // Rust has no safe way to kill the worker; abort is the
            // only deterministic way to free the test runner.
            eprintln!("{} (>{}s): {}", HANG_BANNER_PREFIX, timeout.as_secs(), name);
            eprintln!(
                "watchdog: cannot inspect worker thread stack from std; \
                 printing watchdog-thread backtrace only.\n\
                 Attach a debugger or rerun with `--nocapture` to see live \
                 progress before the next abort."
            );
            eprintln!("Watchdog thread backtrace:\n{}", Backtrace::force_capture());
            // Flush stderr so the banner is not swallowed by the abort.
            use std::io::Write;
            let _ = std::io::stderr().flush();
            std::process::abort();
        }
        Err(RecvTimeoutError::Disconnected) => {
            // Worker dropped the sender without sending — should not
            // happen because send is the last thing it does, but we
            // surface it as a panic rather than a silent pass.
            let _ = worker.join();
            panic!("watchdog: worker thread for `{name}` disconnected without reporting a result");
        }
    }
}

/// Run a test body under a wall-clock watchdog.
///
/// Two forms:
///
/// ```ignore
/// // Default deadline (DEFAULT_TEST_TIMEOUT, 2 minutes).
/// timed_test!(my_test, { /* body */ });
///
/// // Explicit deadline.
/// timed_test!(slow_test, std::time::Duration::from_secs(300), {
///     /* body */
/// });
/// ```
///
/// The generated function is a `#[test]` fn, so it integrates with
/// `cargo test` discovery exactly like a normal test.
#[macro_export]
macro_rules! timed_test {
    ($name:ident, $body:block) => {
        #[test]
        fn $name() {
            $crate::test_util::run_with_watchdog(
                stringify!($name),
                $crate::test_util::DEFAULT_TEST_TIMEOUT,
                || $body,
            );
        }
    };
    ($name:ident, $timeout:expr, $body:block) => {
        #[test]
        fn $name() {
            $crate::test_util::run_with_watchdog(stringify!($name), $timeout, || $body);
        }
    };
}

// ---------------------------------------------------------------------------
// Windows leaked-daemon diagnostic (issue #780).
//
// During PR #779 validation, the soldr-cli lib test binary on Windows
// exited with STATUS_STACK_BUFFER_OVERRUN (0xc0000409) AFTER all
// individual tests reported success. The working hypothesis is that
// one of soldr's spawned subprocesses (zccache-daemon, soldr-daemon)
// keeps a stdio handle inherited from the captured test process
// alive past test teardown, and Windows' fast-fail mechanism trips
// at DLL_PROCESS_DETACH. Issue #692 documents an analogous earlier
// instance with `cli_cargo_native_cc` skips still in place.
//
// `tasklist_snapshot_soldr_daemons` is the diagnostic primitive #780's
// acceptance criteria asked for: a CI-callable helper that surfaces
// any leftover soldr-managed daemon process so a `0xc0000409` exit is
// actionable instead of opaque. The parser is deliberately tolerant
// of `tasklist` quirks ("INFO: No tasks…" header, CSV quoting,
// trailing CR/LF) so a CI snippet like
//
//   if: failure()
//   run: cargo test ... ; powershell -Command "tasklist /FI 'IMAGENAME eq zccache-daemon.exe'"
//
// remains a backstop while the in-process call gives Rust tests the
// same data without shelling out themselves.

/// One row from a `tasklist /FO CSV /NH` enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeakedDaemonInfo {
    pub image_name: String,
    pub pid: u32,
}

/// Names this helper considers "soldr-managed daemons" for the purposes
/// of #780 leak detection. Matched case-insensitively; the `.exe`
/// suffix is required because `tasklist` always reports it on Windows
/// and reporting a Unix-style name here would silently miss real
/// matches.
pub const SOLDR_DAEMON_IMAGE_NAMES: &[&str] = &["zccache-daemon.exe", "soldr-daemon.exe"];

/// Parse the output of `tasklist /FO CSV /NH /FI "IMAGENAME eq <name>"`
/// into a structured list. Tolerant of:
///
/// * the `INFO: No tasks are running which match the specified criteria.`
///   stderr/stdout line that ships when zero matches are found,
/// * extra surrounding whitespace + trailing CRLF,
/// * the standard 5-column CSV (`"name","pid","session","sessno","memuse"`).
///
/// Returns an empty Vec for empty or no-match output rather than
/// panicking so callers can use `.is_empty()` as the "all clean"
/// signal in their assertions.
pub fn parse_tasklist_csv(stdout: &str) -> Vec<LeakedDaemonInfo> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("INFO:") {
            continue;
        }
        // Expected: "name","pid","session","sessno","memuse"
        let mut fields = Vec::new();
        let mut current = String::new();
        let mut in_quote = false;
        for ch in trimmed.chars() {
            match ch {
                '"' => in_quote = !in_quote,
                ',' if !in_quote => {
                    fields.push(std::mem::take(&mut current));
                }
                _ => current.push(ch),
            }
        }
        fields.push(current);
        if fields.len() < 2 {
            continue;
        }
        let image_name = fields[0].trim().to_string();
        let Ok(pid) = fields[1].trim().parse::<u32>() else {
            continue;
        };
        if SOLDR_DAEMON_IMAGE_NAMES
            .iter()
            .any(|known| image_name.eq_ignore_ascii_case(known))
        {
            out.push(LeakedDaemonInfo { image_name, pid });
        }
    }
    out
}

/// Windows-only: enumerate leftover soldr-managed daemons by shelling
/// out to `tasklist`. Returns an empty Vec on non-Windows so callers
/// can write platform-independent diagnostic code that only fires
/// when actually relevant.
#[cfg(target_os = "windows")]
pub fn tasklist_snapshot_soldr_daemons() -> Vec<LeakedDaemonInfo> {
    let mut all = Vec::new();
    for name in SOLDR_DAEMON_IMAGE_NAMES {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {name}"), "/FO", "CSV", "/NH"])
            .output();
        let Ok(output) = output else { continue };
        let stdout = String::from_utf8_lossy(&output.stdout);
        all.extend(parse_tasklist_csv(&stdout));
    }
    all
}

#[cfg(not(target_os = "windows"))]
pub fn tasklist_snapshot_soldr_daemons() -> Vec<LeakedDaemonInfo> {
    Vec::new()
}

/// Render a list of leaked daemons in a CI-friendly multi-line block
/// that points the operator at #780 and #692. Returns `None` when the
/// list is empty so callers can write
/// `if let Some(diag) = format_leaked_daemons(&snap) { panic!("{diag}") }`.
pub fn format_leaked_daemons(daemons: &[LeakedDaemonInfo]) -> Option<String> {
    if daemons.is_empty() {
        return None;
    }
    let mut out = String::from(
        "Detected leftover soldr-managed daemon process(es) after test teardown. \
         This is the leading hypothesis for the Windows lib-test \
         STATUS_STACK_BUFFER_OVERRUN (0xc0000409) fast-fail described in #780. \
         Likely related to #692 stdio-handle inheritance. Suspects:\n",
    );
    for d in daemons {
        out.push_str(&format!("  - {} pid={}\n", d.image_name, d.pid));
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Self-tests for the watchdog itself.
//
// The fast self-tests (panic forwarding, short body completes, custom
// timeout returns normally) always run when the crate's unit tests run.
// The deliberate-hang test is gated behind `#[ignore]` so it does not
// poison normal `cargo test` runs: invoke it manually with
//
//     soldr cargo test -p soldr-cli --lib -- --ignored --nocapture deliberate_hang
//
// to verify the abort path end-to-end.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn watchdog_returns_normally_when_body_finishes_in_time() {
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = Arc::clone(&ran);
        run_with_watchdog("fast_body", Duration::from_secs(30), move || {
            ran_clone.store(true, Ordering::SeqCst);
        });
        assert!(ran.load(Ordering::SeqCst), "body should have run");
    }

    #[test]
    #[should_panic(expected = "intentional")]
    fn watchdog_forwards_panic_from_body() {
        run_with_watchdog("panic_body", Duration::from_secs(30), || {
            panic!("intentional");
        });
    }

    #[test]
    fn watchdog_returns_for_very_short_timeout_when_body_is_instant() {
        // 500ms timeout, instantaneous body. Verifies we don't false-fire
        // on legitimately quick work — the channel send/recv path must
        // beat the timer for noop bodies.
        //
        // Was 1ms originally, but that races on shared GHA ubuntu-24.04
        // runners under parallel test load: worker-thread spawn +
        // channel setup + first `send` can easily take >1ms when the
        // scheduler is busy, tripping the watchdog and calling
        // std::process::abort() on a noop-body test which then bring
        // the whole test binary down (signal 6 SIGABRT). 500ms still
        // catches a real hang — the point of this test is "the fast
        // path exists at all", not µs-precision.
        run_with_watchdog("instant_body", Duration::from_millis(500), || {});
    }

    // -----------------------------------------------------------------
    // Leaked-daemon diagnostic parser (issue #780).
    // -----------------------------------------------------------------

    #[test]
    fn tasklist_parser_extracts_zccache_daemon_row() {
        // Actual `tasklist /FO CSV /NH /FI "IMAGENAME eq zccache-daemon.exe"`
        // output shape from a Windows host with one running daemon.
        let stdout = r#""zccache-daemon.exe","12345","Console","1","8,192 K""#;
        let parsed = parse_tasklist_csv(stdout);
        assert_eq!(
            parsed,
            vec![LeakedDaemonInfo {
                image_name: "zccache-daemon.exe".to_string(),
                pid: 12345,
            }]
        );
    }

    #[test]
    fn tasklist_parser_extracts_soldr_daemon_row() {
        let stdout = r#""soldr-daemon.exe","987","Services","0","4,096 K""#;
        let parsed = parse_tasklist_csv(stdout);
        assert_eq!(
            parsed,
            vec![LeakedDaemonInfo {
                image_name: "soldr-daemon.exe".to_string(),
                pid: 987,
            }]
        );
    }

    #[test]
    fn tasklist_parser_returns_empty_for_no_match_banner() {
        // When zero processes match the filter, tasklist writes its
        // banner to stdout, NOT stderr. Treat it as "all clean".
        let stdout = "INFO: No tasks are running which match the specified criteria.\r\n";
        assert!(parse_tasklist_csv(stdout).is_empty());
    }

    #[test]
    fn tasklist_parser_returns_empty_for_blank_output() {
        assert!(parse_tasklist_csv("").is_empty());
        assert!(parse_tasklist_csv("   \r\n\r\n").is_empty());
    }

    #[test]
    fn tasklist_parser_ignores_unrelated_image_names() {
        // A user might have other long-running processes whose names
        // happen to contain "daemon". The diagnostic must only flag
        // soldr-managed ones to avoid false-positive CI failures.
        let stdout = r#""my-other-daemon.exe","42","Console","1","100 K""#;
        assert!(parse_tasklist_csv(stdout).is_empty());
    }

    #[test]
    fn tasklist_parser_is_case_insensitive_on_image_name() {
        // Some Windows versions report camel-case ImageName values.
        // The diagnostic should fire regardless of case.
        let stdout = r#""ZCcache-Daemon.EXE","7","Console","1","100 K""#;
        let parsed = parse_tasklist_csv(stdout);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].pid, 7);
    }

    #[test]
    fn tasklist_parser_handles_multiple_rows() {
        let stdout = "\"zccache-daemon.exe\",\"1\",\"Console\",\"1\",\"100 K\"\r\n\
                      \"zccache-daemon.exe\",\"2\",\"Console\",\"1\",\"200 K\"\r\n";
        let parsed = parse_tasklist_csv(stdout);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].pid, 1);
        assert_eq!(parsed[1].pid, 2);
    }

    #[test]
    fn tasklist_parser_skips_malformed_pid() {
        // Defensive: a row whose pid field is non-numeric should not
        // panic; it should be silently skipped so the CI signal stays
        // clean if tasklist's format ever changes.
        let stdout = r#""zccache-daemon.exe","not-a-number","Console","1","100 K""#;
        assert!(parse_tasklist_csv(stdout).is_empty());
    }

    #[test]
    fn format_leaked_daemons_returns_none_when_empty() {
        assert!(format_leaked_daemons(&[]).is_none());
    }

    #[test]
    fn format_leaked_daemons_mentions_780_and_692_for_actionability() {
        let formatted = format_leaked_daemons(&[LeakedDaemonInfo {
            image_name: "zccache-daemon.exe".to_string(),
            pid: 4321,
        }])
        .expect("non-empty diagnostic");
        // Acceptance criterion: future 0xc0000409 exits should be
        // actionable. The diagnostic must name the suspect AND point
        // at the open investigation issues so triage starts from a
        // known baseline.
        assert!(formatted.contains("0xc0000409"), "{formatted}");
        assert!(formatted.contains("#780"), "{formatted}");
        assert!(formatted.contains("#692"), "{formatted}");
        assert!(formatted.contains("zccache-daemon.exe"), "{formatted}");
        assert!(formatted.contains("pid=4321"), "{formatted}");
    }

    /// Deliberate hang to exercise the abort path. Marked `#[ignore]`
    /// because a passing run *aborts the test binary* — it cannot
    /// participate in normal `cargo test`. Run it manually:
    ///
    /// ```text
    /// soldr cargo test -p soldr-cli --lib -- --ignored --nocapture deliberate_hang
    /// ```
    ///
    /// Expected: the binary prints `TEST HUNG (>2s): deliberate_hang`
    /// followed by a backtrace, then exits with a non-zero status.
    #[test]
    #[ignore = "aborts the test binary on purpose; run with --ignored to verify watchdog"]
    fn deliberate_hang() {
        run_with_watchdog("deliberate_hang", Duration::from_secs(2), || {
            // Sleep well past the 2s deadline so the watchdog fires.
            std::thread::sleep(Duration::from_secs(300));
        });
    }
}
