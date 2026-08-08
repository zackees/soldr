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

/// Consulted only under the existing `SOLDR_USE_BROKER=1` opt-in -- there is
/// no reason to pay a discovery round-trip against a broker this invocation
/// was never asked to use.
///
/// Returns `true` only when broker discovery confirms a live daemon PID
/// (`DiscoveryRoute::BrokerNegotiated` followed by a successful local probe
/// -- see `broker_discovery::soldr_daemon_pid_via_broker`). Every other
/// outcome (broker disabled, unreachable, refused, or a
/// negotiated-but-unconfirmed backend) returns `false`, leaving the
/// existing legacy-spawn-on-failure behavior exactly unchanged -- this is
/// additive, not a replacement for the direct-connect-first /
/// legacy-spawn-on-failure model.
pub(crate) fn broker_confirmed_daemon_live() -> bool {
    if !crate::broker_spawn::broker_enabled() {
        return false;
    }
    let Ok(paths) = crate::core::SoldrPaths::new() else {
        return false;
    };
    crate::daemon::broker_discovery::soldr_daemon_pid_via_broker(&paths).is_some()
}

/// The `compile_dispatch.rs` cold-connect-failure branch, factored out here
/// so that file (already over the loc_ratchet ceiling) doesn't grow. On a
/// [`broker_confirmed_daemon_live`] hit, the legacy spawn is skipped
/// entirely -- it would be redundant with (and could race) the broker's own
/// singleton bookkeeping -- and the caller's retry loop simply redials the
/// deterministic socket the broker-confirmed daemon is already bound to.
/// Otherwise this is exactly the legacy spawn behavior compile_dispatch has
/// always had.
pub(crate) fn spawn_or_confirm_broker_daemon(
    deadline: std::time::Instant,
) -> (
    Option<crate::daemon::lifecycle::PreparedDaemonSpawn>,
    Option<String>,
) {
    if broker_confirmed_daemon_live() {
        return (None, None);
    }
    match crate::binaries::ensure_daemon_executable_handoff() {
        Ok(_) => match crate::daemon::lifecycle::try_spawn_detached_until(Some(deadline)) {
            Ok(prepared) => (prepared, None),
            Err(error) => (
                None,
                Some(format!("initial daemon spawn failed: {error:?}")),
            ),
        },
        Err(error) => (
            None,
            Some(format!(
                "canonical soldr-daemon handoff materialization failed: {error}"
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // soldr#2364: `broker_confirmed_daemon_live` must degrade to `false`
    // (never panic, never hang) whenever the broker is disabled or
    // unreachable, so the legacy spawn path this dispatch has always used
    // is unaffected unless a front-door-spawned broker actually confirms a
    // live daemon. Proving the `true` branch needs a real broker + a real
    // launched daemon (covered by the Linux Docker harness, not a unit
    // test); these lock down the two safe-degrade branches a unit test
    // *can* exercise without spawning anything.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn set_use_broker(value: Option<&str>) {
        match value {
            Some(v) => std::env::set_var("SOLDR_USE_BROKER", v),
            None => std::env::remove_var("SOLDR_USE_BROKER"),
        }
    }

    crate::timed_test!(
        broker_confirmed_daemon_live_is_false_when_broker_disabled,
        {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            set_use_broker(None);
            assert!(!broker_confirmed_daemon_live());
        }
    );

    crate::timed_test!(
        broker_confirmed_daemon_live_is_false_when_broker_enabled_but_unreachable,
        {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            set_use_broker(Some("1"));
            // No broker is running in the test environment, so discovery
            // must resolve to a fallback route and this must return false,
            // never panic and never hang.
            let result = broker_confirmed_daemon_live();
            set_use_broker(None);
            assert!(!result);
        }
    );
}
