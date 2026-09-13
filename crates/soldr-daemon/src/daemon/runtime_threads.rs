//! Keep the daemon runtime's threads alive for the daemon's whole life (soldr#3210).
//!
//! The embedded zccache service spawns compiler children with running-process
//! `kill_when_owner_dies`. On Linux that is `PR_SET_PDEATHSIG(SIGTERM)`, and
//! the kernel delivers it when the **thread** that forked the child exits,
//! not when the process does.
//!
//! Tokio retires threads. Multi-thread workers run on blocking-pool threads,
//! and `block_in_place` (which zccache uses to hash compile outputs) hands the
//! worker core to a fresh pool thread. The old thread returns to the pool
//! and exits once it has been idle for the keep-alive, 10 s by default. Any
//! compiler it spawned that is still running is then SIGTERMed mid-compile.
//! cook-size-gate's uncapped ci-release build lost `soldr_daemon` this way in
//! 3 of 4 runs, with no memory pressure at all.
//!
//! A pool thread exits only on keep-alive expiry or runtime shutdown, so an
//! unbounded keep-alive removes the first cause. Idle threads then park
//! instead of exiting, and the pool never grows past its existing
//! `max_blocking_threads` bound. Children still die with the daemon, which
//! is what the owner-death primitive is for.

use std::time::Duration;

/// Effectively forever: longer than any daemon lives, yet small enough that
/// no platform's condvar timeout arithmetic can overflow on it.
pub(crate) const DAEMON_THREAD_KEEP_ALIVE: Duration = Duration::from_secs(u32::MAX as u64);

/// Apply the thread-lifetime policy to a daemon runtime builder.
pub(crate) fn keep_threads_for_daemon_lifetime(
    builder: &mut tokio::runtime::Builder,
) -> &mut tokio::runtime::Builder {
    builder.thread_keep_alive(DAEMON_THREAD_KEEP_ALIVE)
}

#[cfg(test)]
#[path = "runtime_threads_tests.rs"]
mod tests;
