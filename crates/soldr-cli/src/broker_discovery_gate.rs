//! soldr#2364: give a front-door-spawned broker a chance to serve a compile
//! before `compile_dispatch.rs` falls to the legacy direct-spawn path.
//!
//! Split into its own module rather than grown inline in
//! `compile_dispatch.rs` (already over the repo's 1,500-line loc_ratchet
//! ceiling, which only allows a file already over the ceiling to shrink,
//! never grow).
//!
//! Root-caused on the Linux Docker harness: `compile_dispatch.rs`'s
//! `dispatch_compile_with_sock_and_marker_detailed` -- the function every
//! real compile goes through -- never called `lifecycle::is_live` /
//! `broker_discovery::discover_via_broker` at all. It dialed
//! `client::default_sock_path` directly and, on failure, went straight to
//! the legacy `try_spawn_detached_until`. Confirmed by running a foreground
//! `RUST_LOG=debug soldr broker serve` and a real cargo build against it:
//! the broker's log showed only its own startup lines, no Hello ever
//! arrived. This meant the front-door broker-program-namespace fix
//! (soldr#2379), while correct, could never actually route a real build
//! through a front-door-spawned broker: nothing tried.

/// Returns `true` only when broker discovery confirms a live daemon PID
/// (`DiscoveryRoute::BrokerNegotiated` followed by a successful local probe
/// -- see `broker_discovery::soldr_daemon_pid_via_broker`). Every other
/// outcome (broker unreachable, refused, or a negotiated-but-unconfirmed
/// backend) returns `false`, leaving the legacy-spawn-on-failure behavior
/// unchanged -- this is additive, not a replacement for the
/// direct-connect-first / legacy-spawn-on-failure model.
pub(crate) fn broker_confirmed_daemon_live() -> bool {
    let Ok(paths) = crate::core::SoldrPaths::new() else {
        return false;
    };
    crate::daemon::broker_discovery::soldr_daemon_pid_via_broker(&paths).is_some()
}

/// soldr#2388 Step 4: the client **never** spawns the daemon — the broker is
/// the sole daemon-spawner (it materializes the daemon image and launches it at
/// startup; see `broker_cmd.rs`). The compile hot path only *dials* the
/// daemon's deterministic socket; on a cold-connect failure the caller's retry
/// loop simply redials until the broker-launched daemon binds, or the retry
/// budget expires and the failure is surfaced as an infra-attributed hard error
/// with a remedy (`daemon_infra_remedy`) — never a client spawn, never a silent
/// uncached rustc. Returns `(None, None)`: no prepared spawn, no spawn error.
pub(crate) fn spawn_or_confirm_broker_daemon(
    _deadline: std::time::Instant,
) -> (
    Option<crate::daemon::lifecycle::PreparedDaemonSpawn>,
    Option<String>,
) {
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // soldr#2364/#2388: `broker_confirmed_daemon_live` must degrade to `false`
    // (never panic, never hang) when no broker is reachable, so the legacy
    // spawn path is unaffected unless a front-door-spawned broker actually
    // confirms a live daemon. Proving the `true` branch needs a real broker +
    // a real launched daemon (covered by the real-process integration harness
    // `session_multiprocess_smoke`); this locks down the safe-degrade branch a
    // unit test can exercise without spawning anything.
    crate::timed_test!(
        broker_confirmed_daemon_live_is_false_when_broker_unreachable,
        {
            // No broker is running in the test environment, so discovery must
            // resolve to a fallback route and this must return false, never
            // panic and never hang.
            assert!(!broker_confirmed_daemon_live());
        }
    );
}
