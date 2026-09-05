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
//!
//! # PID-Key, not bare PID
//!
//! A handle-based liveness probe closes one reuse window but not the one
//! that matters here: the requester table is a long-lived map keyed on a
//! `u32` the kernel is free to hand to an unrelated process the moment the
//! original requester exits. A bare-PID table would then read that unrelated
//! process's liveness as proof the original requester is still around, and
//! keep a route alive (or worse, in a differently-shaped table, direct an
//! action at the wrong process) indefinitely.
//!
//! The fix is the same identity pairing `broker_lease` and
//! `terminate::is_same_process` already use for the same reason: `(pid,
//! start_token)`, where `start_token` is the process's creation time,
//! resolved through `platform::process::inspect::process_start_token` *at
//! the moment the requester is recorded*, while it is known to be alive
//! because it is the peer of the connection making the request. Liveness
//! later means "a process with this pid exists AND its start token still
//! equals the recorded one" -- see [`requester_is_alive`].
//!
//! A start token that cannot be resolved at record time is not treated as
//! "assume dead" or "assume alive": it is [`RequesterKey::Unidentified`],
//! and a route holding one is permanently exempt from [`RouteVerdict::Reap`].
//! See that variant's docs for why.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use running_process::broker::server::BackendRegistry;

/// A PID paired with the process-creation token resolved when it was
/// recorded, so a later kernel PID reuse cannot be mistaken for the same
/// process. See the module-level "PID-Key, not bare PID" section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PidKey {
    pub(crate) pid: u32,
    pub(crate) start_token: u64,
}

/// What was captured for one route request.
///
/// `Unidentified` covers a pid whose start token could not be resolved at
/// record time -- the connecting process raced its own exit, or the
/// platform probe failed for some other reason. Its pid is deliberately not
/// retained: without a token there is no way to tell this pid apart from an
/// unrelated later process that reuses the same number, so nothing safe can
/// be done with it except never claim the route it is part of has emptied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequesterKey {
    /// A requester whose `(pid, start_token)` identity is known.
    Identified(PidKey),
    /// A requester recorded, but whose identity could not be resolved.
    Unidentified,
}

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
    /// At least one requester is alive, or a requester was recorded whose
    /// identity could not be resolved. Nothing to do either way -- see
    /// [`RequesterKey::Unidentified`] for why the second case must never
    /// progress to [`RouteVerdict::Reap`], however long it persists.
    Live,
    /// Every requester is gone, but the grace window has not elapsed.
    Draining,
    /// Every requester is gone and the grace window has elapsed.
    Reap,
}

/// The requesters of one route, and when one of them was last known alive.
#[derive(Debug, Clone)]
pub(crate) struct RouteOwners {
    requesters: BTreeSet<PidKey>,
    /// Sticky once set: this route has, at some point, recorded a requester
    /// whose start token could not be resolved. There is no way to later
    /// prove that unnamed process exited, so this can never be cleared and
    /// the route can never reach `Reap` while it is set. Killing a daemon
    /// whose owner cannot be re-identified is worse than leaking one.
    has_unidentified_requester: bool,
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
    fn new(first: RequesterKey, now: Instant) -> Self {
        let mut owners = Self {
            requesters: BTreeSet::new(),
            has_unidentified_requester: false,
            last_live_at: now,
        };
        owners.record(first);
        owners
    }

    fn record(&mut self, key: RequesterKey) {
        match key {
            RequesterKey::Identified(pid_key) => {
                self.requesters.insert(pid_key);
            }
            RequesterKey::Unidentified => {
                self.has_unidentified_requester = true;
            }
        }
    }

    /// Identified-requester count, for diagnostics and tests. Does not
    /// reflect an unidentified requester, which is never added to the set --
    /// see [`RouteOwners::has_unidentified_requester`].
    pub(crate) fn len(&self) -> usize {
        self.requesters.len()
    }

    /// Whether this route has ever recorded a requester whose identity could
    /// not be resolved. For diagnostics and tests.
    pub(crate) fn has_unidentified_requester(&self) -> bool {
        self.has_unidentified_requester
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

    /// Record that a requester asked for `service_name`.
    ///
    /// `key` must already be resolved by the caller -- see the module-level
    /// "PID-Key, not bare PID" docs for why resolution has to happen at the
    /// moment of the request, while the requester is known to be alive,
    /// rather than lazily inside this method or later at sweep time. By the
    /// time a sweep runs, the requester may already be gone and its start
    /// token unrecoverable, which would make "cannot resolve now" (a
    /// judgement call about the present) indistinguishable from "was never
    /// resolvable" (a fact about the past) -- exactly the ambiguity
    /// `RequesterKey::Unidentified` exists to keep out of this table.
    ///
    /// A route accumulates requesters over its life. The canonical route for
    /// the user's own root is asked for by every build, so it keeps acquiring
    /// live requesters and never reaches an empty set for long; a fixture
    /// route is asked for exactly once, by a process that will not come back.
    /// The same rule therefore covers both without needing to tell them apart
    /// by path, which would be a heuristic about directory names.
    pub(crate) fn record_request(&mut self, service_name: &str, key: RequesterKey, now: Instant) {
        match self.routes.get_mut(service_name) {
            Some(owners) => {
                owners.record(key);
                owners.last_live_at = now;
            }
            None => {
                self.routes
                    .insert(service_name.to_string(), RouteOwners::new(key, now));
            }
        }
    }

    /// Drop requesters that have exited and classify each route.
    ///
    /// `is_alive` is injected so the decision logic is testable without
    /// spawning processes; production passes the handle-based, token-checking
    /// probe ([`requester_is_alive`]).
    pub(crate) fn sweep(
        &mut self,
        now: Instant,
        grace: Duration,
        mut is_alive: impl FnMut(PidKey) -> bool,
    ) -> Vec<(String, RouteVerdict)> {
        let mut verdicts = Vec::with_capacity(self.routes.len());
        for (service_name, owners) in &mut self.routes {
            owners.requesters.retain(|key| is_alive(*key));
            let verdict = if owners.has_unidentified_requester {
                // Never reap: see `RouteOwners::has_unidentified_requester`
                // and `RequesterKey::Unidentified`. Treated the same as a
                // live requester for the timestamp too, so a route that
                // later does resolve every other requester does not fall
                // straight into `Reap` on the strength of a stale timestamp.
                owners.last_live_at = now;
                RouteVerdict::Live
            } else if owners.requesters.is_empty() {
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

/// Resolve `pid` into a [`RequesterKey`] right now.
///
/// Kept here, not inlined at the one call site (`BrokerState::
/// record_route_request`), so the "resolve immediately, while the peer is
/// known alive" contract from the module-level "PID-Key, not bare PID" docs
/// has a single named place to read and to change.
pub(crate) fn resolve_requester_key(pid: u32) -> RequesterKey {
    match crate::platform::process::inspect::process_start_token(pid) {
        Some(start_token) => RequesterKey::Identified(PidKey { pid, start_token }),
        None => RequesterKey::Unidentified,
    }
}

/// Liveness through `running_process`'s handle-based probe, corroborated by
/// the PID-Key: the pid must both be alive AND its current start token must
/// still equal the one resolved when the requester was recorded. Either
/// check alone is reuse-vulnerable in a different way -- a handle-based
/// liveness probe on a bare pid still cannot tell "the process I remember"
/// from "a different process the kernel later gave the same number" -- so
/// both must hold.
///
/// Kept as a named function so the reaper's production wiring names the
/// reuse-safe path explicitly, and so a future move to `kernal-api` has one
/// call site to change (zackees/kernal-api#67).
pub(crate) fn requester_is_alive(key: PidKey) -> bool {
    running_process::broker::backend_lifecycle::verify_pid::process_is_alive(key.pid)
        && crate::platform::process::inspect::process_start_token(key.pid) == Some(key.start_token)
}

/// How often the broker looks for routes whose requesters have all exited.
///
/// Well below the grace window, so the reap lands within one interval of the
/// window elapsing rather than one interval after it.
pub(crate) const REAP_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Note that `pid` asked for `service_name`.
///
/// The pid comes from `PeerIdentity`, which the accept loop reads from
/// platform IPC credentials, so it identifies the process on the other
/// end of this socket and cannot be spoofed by its payload.
pub(crate) fn record_route_request(
    route_owners: &Mutex<RouteOwnership>,
    service_name: &str,
    pid: u32,
) {
    let key = resolve_requester_key(pid);
    route_owners
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .record_request(service_name, key, Instant::now());
}

/// Stop the daemons of routes whose requesters have all exited.
///
/// Returns the service names reaped, for the log line and for tests.
/// Termination is graceful first: a daemon asked to stop flushes its
/// caches, and a force-kill that skipped that step would trade a leaked
/// process for a corrupted one.
pub(crate) fn reap_orphaned_routes(
    route_owners: &Mutex<RouteOwnership>,
    registry: &Mutex<BackendRegistry>,
    grace: Duration,
) -> Vec<String> {
    use running_process::broker::backend_lifecycle::verify_pid::signal_terminate;

    let verdicts = {
        let mut owners = route_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        owners.sweep(Instant::now(), grace, requester_is_alive)
    };

    let mut reaped = Vec::new();
    for (service_name, verdict) in verdicts {
        if verdict != RouteVerdict::Reap {
            continue;
        }
        // Read the daemon pid under the registry lock, then release it
        // before signalling: the signal is a syscall on another process
        // and must not be holding the lock every route request needs.
        let daemon_pid: Option<u32> = {
            let registry = registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let found = registry.iter().find_map(|(key, handle)| {
                (key.service_name == service_name).then_some(handle.daemon_process.pid)
            });
            found
        };
        if let Some(pid) = daemon_pid {
            let _ = signal_terminate(pid);
        }
        // Forget the route either way. With no live requester and no
        // registry entry there is nothing left to tear down, and keeping
        // the row would make a long-lived broker accumulate one entry per
        // fixture that ever ran.
        route_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .forget(&service_name);
        {
            let mut registry = registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.prune_stale();
        }
        reaped.push(service_name);
    }
    reaped
}

/// Periodically stop the daemons of routes nobody is left to use.
///
/// Runs in the broker rather than in each daemon because the broker is the
/// only process that knows every route, holds kernel-supplied identities for
/// the callers that asked for them, and is the parent that can act. See the
/// module-level docs above for why no Unix mechanism can do this without a
/// watcher.
pub(crate) async fn run_route_reaper(
    route_owners: Arc<Mutex<RouteOwnership>>,
    registry: Arc<Mutex<BackendRegistry>>,
    shutdown: Arc<tokio::sync::Notify>,
) {
    let grace = DEFAULT_GRACE;
    loop {
        tokio::select! {
            _ = shutdown.notified() => return,
            _ = tokio::time::sleep(REAP_SWEEP_INTERVAL) => {}
        }
        // The sweep signals other processes, so keep it off the async worker.
        let sweep_owners = Arc::clone(&route_owners);
        let sweep_registry = Arc::clone(&registry);
        let reaped = tokio::task::spawn_blocking(move || {
            reap_orphaned_routes(&sweep_owners, &sweep_registry, grace)
        })
        .await
        .unwrap_or_default();
        for service_name in reaped {
            println!(
                "soldr broker: reaped route {service_name}; every process that asked for it has exited"
            );
        }
    }
}

#[cfg(test)]
#[path = "broker_reaper_tests.rs"]
mod tests;
