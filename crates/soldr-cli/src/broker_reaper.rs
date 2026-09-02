//! Owner-death reaping for broker routes (soldr#3054).
//!
//! # Why the broker owns this
//!
//! A broker route is keyed on the canonicalized soldr root, so every caller
//! with its own `SOLDR_CACHE_DIR` -- every test fixture, notably -- gets a
//! route and a daemon of its own. When such a caller is killed rather than
//! allowed to shut down, nothing ever stops that daemon. One measured run of
//! this repository's suite left 63 resident daemons behind.
//!
//! The daemon can watch its own owner (`soldr-daemon --owner-pid`), and does,
//! but the broker is the better place for four reasons:
//!
//! 1. **It already knows who asked.** `PeerIdentity.pid` comes from platform
//!    IPC credentials -- `SO_PEERCRED` on Unix, `GetNamedPipeClientProcessId`
//!    on Windows. Nothing is declared by the caller, so nothing can be
//!    mis-declared or lost when an environment variable fails to propagate.
//! 2. **One watcher instead of N.** Sixty-three daemons each running a timer
//!    to answer one question is sixty-three timers.
//! 3. **It is the parent.** It can signal the daemon and, on Unix, reap it.
//!    A self-watch cannot help with either.
//! 4. **A wedged daemon is exactly the one a self-watch cannot reap**, and
//!    exactly the one worth reaping.
//!
//! # Why Unix forces this shape
//!
//! There is no general Unix mechanism by which the kernel kills process B
//! when unrelated process A exits. `PR_SET_PDEATHSIG` binds a process to its
//! own immediate parent -- which for a broker-launched daemon is the broker,
//! not the caller -- and additionally fires on parent *thread* exit. A PID
//! namespace does bind lifetimes in the kernel, but only to the namespace's
//! own init, which means restructuring the process tree. `cgroup.kill` and
//! FreeBSD's reaper make a kill reliable once something decides to issue it;
//! neither makes it automatic. Windows alone can enforce the binding, through
//! a job object.
//!
//! So on two of three platforms a watcher is not a fallback, it is the
//! implementation, and it belongs somewhere singular and long-lived. That is
//! the broker.
//!
//! # Portability
//!
//! Liveness and signalling go through `running_process`, whose
//! `verify_pid::process_is_alive` opens a handle rather than testing a number
//! -- a pidfd on Linux, a kqueue subscription on macOS, a process handle on
//! Windows. A handle cannot silently retarget when the kernel reuses a PID,
//! which a bare `kill(pid, 0)` poll can. No `cfg` fan-out appears in this
//! module.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// How long a route with no live requester is kept before it is reaped.
///
/// Not zero. A build that finishes and immediately starts another would
/// otherwise pay a daemon start each time, and a cold daemon start pays a
/// full executable image hash (soldr#2517), which is measurable. The grace
/// window costs an idle daemon for its duration and saves that hash for every
/// caller that comes back inside it.
pub(crate) const DEFAULT_GRACE: Duration = Duration::from_secs(120);

/// What a route's requesters look like right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RouteVerdict {
    /// At least one requester is alive. Nothing to do.
    Live,
    /// Every requester is gone, but the grace window has not elapsed.
    Draining,
    /// Every requester is gone and the grace window has elapsed.
    Reap,
}

/// The requesters of one route, and when one of them was last known alive.
#[derive(Debug, Clone)]
pub(crate) struct RouteOwners {
    requesters: BTreeSet<u32>,
    /// The most recent moment this route was known to have a live requester.
    ///
    /// Deliberately not "when the route became empty", which the broker
    /// cannot know: a requester dies between sweeps and leaves no timestamp
    /// behind. Measuring from the last *observation* of liveness bounds the
    /// error at one sweep interval and never reaps early. Measuring from the
    /// first sweep that noticed emptiness would restart the clock on every
    /// tick of a broker that sweeps more often than the grace window, and
    /// nothing would ever be reaped.
    last_live_at: Instant,
}

impl RouteOwners {
    fn new(first: u32, now: Instant) -> Self {
        Self {
            requesters: BTreeSet::from([first]),
            last_live_at: now,
        }
    }

    /// Requester count, for diagnostics and tests.
    pub(crate) fn len(&self) -> usize {
        self.requesters.len()
    }
}

/// Per-route requester table.
///
/// Keyed by service name, which is what both the registry and the Hello
/// payload agree on.
#[derive(Debug, Default)]
pub(crate) struct RouteOwnership {
    routes: BTreeMap<String, RouteOwners>,
}

impl RouteOwnership {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record that `pid` asked for `service_name`.
    ///
    /// A route accumulates requesters over its life. The canonical route for
    /// the user's own root is asked for by every build, so it keeps acquiring
    /// live requesters and never reaches an empty set for long; a fixture
    /// route is asked for exactly once, by a process that will not come back.
    /// The same rule therefore covers both without needing to tell them apart
    /// by path, which would be a heuristic about directory names.
    pub(crate) fn record_request(&mut self, service_name: &str, pid: u32, now: Instant) {
        match self.routes.get_mut(service_name) {
            Some(owners) => {
                owners.requesters.insert(pid);
                owners.last_live_at = now;
            }
            None => {
                self.routes
                    .insert(service_name.to_string(), RouteOwners::new(pid, now));
            }
        }
    }

    /// Drop requesters that have exited and classify each route.
    ///
    /// `is_alive` is injected so the decision logic is testable without
    /// spawning processes; production passes the handle-based probe.
    pub(crate) fn sweep(
        &mut self,
        now: Instant,
        grace: Duration,
        mut is_alive: impl FnMut(u32) -> bool,
    ) -> Vec<(String, RouteVerdict)> {
        let mut verdicts = Vec::with_capacity(self.routes.len());
        for (service_name, owners) in &mut self.routes {
            owners.requesters.retain(|pid| is_alive(*pid));
            let verdict = if owners.requesters.is_empty() {
                if now.saturating_duration_since(owners.last_live_at) >= grace {
                    RouteVerdict::Reap
                } else {
                    RouteVerdict::Draining
                }
            } else {
                owners.last_live_at = now;
                RouteVerdict::Live
            };
            verdicts.push((service_name.clone(), verdict));
        }
        verdicts
    }

    /// Forget a route once its daemon is gone.
    pub(crate) fn forget(&mut self, service_name: &str) {
        self.routes.remove(service_name);
    }

    /// Requester table for one route, for diagnostics and tests.
    pub(crate) fn owners_of(&self, service_name: &str) -> Option<&RouteOwners> {
        self.routes.get(service_name)
    }

    /// Number of routes being tracked.
    pub(crate) fn len(&self) -> usize {
        self.routes.len()
    }
}

/// Liveness through `running_process`'s handle-based probe.
///
/// Kept as a named function so the reaper's production wiring names the
/// reuse-safe path explicitly, and so a future move to `kernal-api` has one
/// call site to change (zackees/kernal-api#67).
pub(crate) fn requester_is_alive(pid: u32) -> bool {
    running_process::broker::backend_lifecycle::verify_pid::process_is_alive(pid)
}

#[cfg(test)]
#[path = "broker_reaper_tests.rs"]
mod tests;
