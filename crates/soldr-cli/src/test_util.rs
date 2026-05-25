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
        // 1ms timeout, instantaneous body. Verifies we don't false-fire
        // on legitimately quick work — the channel send/recv path must
        // beat the timer for noop bodies.
        run_with_watchdog("instant_body", Duration::from_millis(1), || {});
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
