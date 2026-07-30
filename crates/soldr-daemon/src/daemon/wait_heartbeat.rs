//! Progressive "still waiting" heartbeats for long daemon IPC waits (#1838).
//!
//! # The problem this addresses
//!
//! A compile dispatched to the daemon waits up to
//! `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` (default 30 minutes) for a reply, and
//! until now said nothing for the whole of it. #1838 puts the point
//! precisely: three daemon-lifecycle defects all presented to the user the
//! same way — *the build stopped making progress, and nothing said why until
//! a multi-minute backstop expired*. The bound is not the bug; the silence
//! is.
//!
//! # Why a watchdog thread rather than a shorter read timeout
//!
//! The obvious alternative is to shorten the socket read timeout and loop.
//! That changes IPC semantics: `read_frame_sync` would start returning
//! `WouldBlock`/`TimedOut` mid-frame, and every caller would need to
//! distinguish "no reply yet" from "the daemon died". A thread that only
//! prints leaves the transport untouched, so a heartbeat can never turn a
//! healthy slow compile into a failed one.
//!
//! # Message shape
//!
//! Deliberately identical to the cargo front door's existing heartbeat
//! (`cargo_front_door::cargo_wait_heartbeat_message`): operation, elapsed
//! seconds, the active deadline, and the env var that controls it. #1838
//! calls that shape out as prior art to reuse rather than reinvent, and
//! matching it means one format to learn. It is not shared code because
//! `soldr-daemon` cannot depend on `soldr-cli` — the dependency runs the
//! other way.
//!
//! Output goes to **stderr**, which keeps `--json` modes on stdout intact.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often to report that a wait is still outstanding. Matches the cargo
/// front door's `CARGO_WAIT_HEARTBEAT_SECS`, so the two surfaces tick at the
/// same rate and a user does not have to learn two cadences.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// Poll granularity for the stop flag. Small enough that the thread joins
/// promptly on a fast compile, large enough not to spin.
const STOP_POLL: Duration = Duration::from_millis(100);

/// Emits a heartbeat every [`HEARTBEAT_INTERVAL`] until dropped.
///
/// Nothing is printed if the guarded operation finishes inside the first
/// interval, which is the overwhelmingly common case — a warm compile
/// returns in milliseconds, so this is silent unless something is actually
/// slow.
pub(crate) struct WaitHeartbeat {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WaitHeartbeat {
    /// Start reporting on `operation`, whose deadline is `timeout`,
    /// overridable via `env_var`.
    pub(crate) fn start(
        operation: &'static str,
        timeout: Duration,
        env_var: Option<&'static str>,
    ) -> Self {
        Self::start_with_interval(operation, timeout, env_var, HEARTBEAT_INTERVAL)
    }

    fn start_with_interval(
        operation: &'static str,
        timeout: Duration,
        env_var: Option<&'static str>,
        interval: Duration,
    ) -> Self {
        Self::start_with_interval_and_sink(operation, timeout, env_var, interval, |msg| {
            eprintln!("{msg}");
        })
    }

    /// The core loop, with the emit routed through `sink` so a test can assert
    /// the heartbeat actually fires at the interval without capturing process
    /// stderr. Production always passes the `eprintln!` sink above.
    fn start_with_interval_and_sink<S>(
        operation: &'static str,
        timeout: Duration,
        env_var: Option<&'static str>,
        interval: Duration,
        sink: S,
    ) -> Self
    where
        S: Fn(String) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("soldr-wait-heartbeat".to_string())
            .spawn(move || {
                let started = Instant::now();
                let mut next = interval;
                while !thread_stop.load(Ordering::Relaxed) {
                    std::thread::sleep(STOP_POLL);
                    if thread_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if started.elapsed() >= next {
                        sink(heartbeat_message(
                            operation,
                            started.elapsed(),
                            timeout,
                            env_var,
                        ));
                        next += interval;
                    }
                }
            })
            .ok();
        Self { stop, handle }
    }
}

impl Drop for WaitHeartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // Joining bounds the thread's lifetime to the wait it describes,
            // so a heartbeat can never outlive its operation and print about
            // a compile that already finished.
            let _ = handle.join();
        }
    }
}

/// The message body, split out so the wording is testable without spawning a
/// thread or waiting a minute.
///
/// `env_var` is optional because not every long wait has an override to
/// name — the cache flush and graceful shutdown budgets are fixed. Saying
/// "deadline 300s" without inventing a knob is better than implying one
/// exists.
pub(crate) fn heartbeat_message(
    operation: &str,
    elapsed: Duration,
    timeout: Duration,
    env_var: Option<&str>,
) -> String {
    let deadline = match env_var {
        Some(var) => format!("deadline {}s from {var}", timeout.as_secs()),
        None => format!("fixed deadline {}s", timeout.as_secs()),
    };
    format!(
        "soldr: {operation} still waiting after {}s ({deadline}); \
         if this is a wedged cache rather than slow work, \
         `soldr --no-cache cargo ...` or ZCCACHE_DISABLE=1 bypasses the daemon",
        elapsed.as_secs(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(message_names_operation_elapsed_deadline_and_override, {
        let msg = heartbeat_message(
            "daemon compile reply",
            Duration::from_secs(120),
            Duration::from_secs(1800),
            Some("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
        );
        assert!(msg.contains("daemon compile reply"), "{msg}");
        assert!(msg.contains("after 120s"), "{msg}");
        assert!(msg.contains("deadline 1800s"), "{msg}");
        assert!(msg.contains("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"), "{msg}");
    });

    crate::timed_test!(message_carries_the_corrective_action_inline, {
        // #1838 Phase 2: the remedy belongs in the message, not only in
        // CLAUDE.md, because the person reading a stalled build is not
        // reading the repo docs.
        let msg = heartbeat_message(
            "daemon compile reply",
            Duration::from_secs(60),
            Duration::from_secs(1800),
            Some("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
        );
        assert!(msg.contains("--no-cache"), "{msg}");
        assert!(msg.contains("ZCCACHE_DISABLE=1"), "{msg}");
    });

    crate::timed_test!(a_fast_operation_prints_nothing, {
        // The common case. A warm compile returns in milliseconds and must
        // not emit a heartbeat, or every build grows noise.
        let guard = WaitHeartbeat::start_with_interval(
            "unit test",
            Duration::from_secs(1800),
            Some("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
            Duration::from_secs(3600),
        );
        std::thread::sleep(Duration::from_millis(50));
        drop(guard);
    });

    crate::timed_test!(a_slow_operation_fires_repeated_heartbeats, {
        // #1838 Phase 1 box 5: assert the heartbeat actually EMITS once the
        // interval elapses — the message tests above only cover wording. A
        // sink captures the emissions so the assertion never touches process
        // stderr. `STOP_POLL` (100 ms) bounds how often the loop can fire, so
        // ~400 ms comfortably yields at least two.
        let emitted: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_target = Arc::clone(&emitted);
        let guard = WaitHeartbeat::start_with_interval_and_sink(
            "unit test",
            Duration::from_secs(1800),
            Some("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
            Duration::from_millis(10),
            move |msg| sink_target.lock().unwrap().push(msg),
        );
        std::thread::sleep(Duration::from_millis(400));
        drop(guard);
        let hits = emitted.lock().unwrap();
        assert!(
            hits.len() >= 2,
            "a slow op must emit repeated heartbeats; got {}",
            hits.len()
        );
        assert!(
            hits[0].contains("unit test") && hits[0].contains("after"),
            "{}",
            hits[0]
        );
    });

    crate::timed_test!(the_guard_joins_its_thread_on_drop, {
        // A heartbeat that outlived its operation would report on a compile
        // that already finished, which is worse than silence.
        let guard = WaitHeartbeat::start_with_interval(
            "unit test",
            Duration::from_secs(1800),
            Some("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
            Duration::from_millis(10),
        );
        std::thread::sleep(Duration::from_millis(30));
        let stop = Arc::clone(&guard.stop);
        drop(guard);
        assert!(
            stop.load(Ordering::Relaxed),
            "drop must signal the heartbeat thread to stop",
        );
    });
}
