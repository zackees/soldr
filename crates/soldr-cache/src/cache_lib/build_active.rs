//! Process-wide "build active" flag (issue #980, L7).
//!
//! The flag lets long-running background workers — auto-GC's
//! orchestrator and (eventually) the inotify cache watcher — skip a
//! tick when a `cargo build` is currently running in the same process.
//! It is set on `BuildSessionStart` and cleared on `BuildSessionEnd`
//! by both the cargo-front-door dispatch path and the daemon's IPC
//! handler, so every soldr process that participates in a build sees
//! the same state without holding a lock.
//!
//! The flag is a `static AtomicBool` so all threads in the process
//! observe the same value. It is NOT shared across processes — the
//! cargo-wrapper process and the daemon process each maintain their
//! own copy, kept in sync via the existing `BuildSessionStart` /
//! `BuildSessionEnd` IPC. That suffices because the workers we are
//! gating run inside whichever process owns them.
//!
//! Default: `false` (idle = no build active = workers run normally).

use std::sync::atomic::{AtomicBool, Ordering};

static IS_BUILD_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mark the process as inside an active build session. Call from
/// `BuildSessionStart` handlers (both the cargo front door's local
/// stamp and the daemon-side IPC dispatcher).
pub fn set(active: bool) {
    IS_BUILD_ACTIVE.store(active, Ordering::Release);
}

/// True iff a build session is currently active in this process.
/// Auto-GC and the inotify watcher consult this at the top of each
/// tick and bail out if the build hasn't ended yet.
pub fn is_active() -> bool {
    IS_BUILD_ACTIVE.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(set_clear_round_trip, {
        // The static is process-wide so we can't strictly observe the
        // boot value if a prior test mutated it. Cycle through the
        // two states and verify each store is observable.
        set(true);
        assert!(is_active());
        set(false);
        assert!(!is_active());
    });
}
