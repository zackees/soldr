//! One cooperative shutdown signal, broadcast to every waiter (soldr#3158).
//!
//! Long-running soldr processes (the daemon, the broker) fan a single "stop
//! now" event out to several independent loops: an accept loop, a route
//! reaper, an RSS watchdog, a maintenance scheduler. A bare
//! [`tokio::sync::Notify`] is the wrong primitive for that shape, and gets it
//! wrong *silently*:
//!
//! * `notify_one()` wakes exactly **one** waiter. With N loops parked on the
//!   same `Notify`, N-1 of them never learn the process is shutting down —
//!   including, in soldr#3158, the accept loop that owns the exit. The
//!   observable symptom is not an error: the cooperative request is accepted,
//!   the process simply never exits, and the caller's drain deadline expires
//!   and force-kills it. Every stop looked like a hung drain.
//! * `notify_waiters()` wakes all of them but stores **no permit**, so a
//!   request that lands before a loop registers is lost forever.
//!
//! [`ShutdownSignal`] pairs `notify_waiters()` with a latching flag, which is
//! what makes it correct for *both* hazards and cancel-safe inside a
//! `tokio::select!`: a waiter dropped mid-`wait()` (because a sibling select
//! branch won) re-reads the flag on its next pass instead of parking on a
//! notification that already happened.
//!
//! Extracted from `daemon::maintenance`, where it was written for the
//! maintenance scheduler, once the broker needed the same guarantee.
//! `maintenance` re-exports it, so the original path still resolves.

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// A latching, broadcast, one-shot shutdown signal.
///
/// Share it as an `Arc<ShutdownSignal>`: every loop calls [`wait`] in its
/// `select!`, and whoever observes the stop request calls [`request`] once.
///
/// [`wait`]: ShutdownSignal::wait
/// [`request`]: ShutdownSignal::request
#[derive(Default, Debug)]
pub struct ShutdownSignal {
    requested: AtomicBool,
    notify: Notify,
}

impl ShutdownSignal {
    /// Latch the request and wake **every** waiter. Idempotent.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Whether [`request`](Self::request) has been called.
    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Resolve once shutdown has been requested — immediately if it already
    /// has been. Cancel-safe: dropping the future loses nothing, because the
    /// next call re-reads the latched flag.
    pub async fn wait(&self) {
        loop {
            // Register with the Notify BEFORE re-checking the flag.
            //
            // `notify_waiters()` stores no permit: a `Notified` future
            // snapshots the waiter generation when it is *enabled*, so the
            // naive `while !is_requested() { notified().await }` loses the
            // wakeup for this interleaving and parks forever —
            //
            //   waiter:    is_requested() -> false
            //   requester: store(true); notify_waiters()
            //   waiter:    notified().await   <- missed it, never re-checks
            //
            // Enabling first means a `request()` landing anywhere after this
            // point either sets the flag we are about to read, or wakes the
            // future we already registered.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
#[path = "shutdown_signal_tests.rs"]
mod tests;
