//! Idle stand-down for the broker (soldr#3193).
//!
//! A broker outlives every client that ever used it. That is the design --
//! it is the stable singleton the front door finds -- but under a throwaway
//! `HOME` it means one broker per test fixture, forever, as long as the
//! fixture directory exists (the image watch of soldr#3184 only fires when
//! the install directory is deleted). Nothing distinguishes a fixture broker
//! from a real one except that nobody comes back: so a broker that has
//! served no route and held no connection for [`DEFAULT_IDLE_EXIT`] stands
//! itself down. The front door respawns the real one in well under a second
//! on the next invocation, and daemons it owns go with it (soldr#3183).
//!
//! `SOLDR_BROKER_IDLE_EXIT_SECS` overrides the window; `0` disables it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::daemon::shutdown_signal::ShutdownSignal;

pub(crate) const IDLE_EXIT_ENV: &str = "SOLDR_BROKER_IDLE_EXIT_SECS";
pub(crate) const DEFAULT_IDLE_EXIT: Duration = Duration::from_secs(30 * 60);
pub(crate) const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(15);

/// The configured idle window; `None` when idle exit is disabled.
pub(crate) fn idle_exit_window() -> Option<Duration> {
    idle_exit_window_for(std::env::var(IDLE_EXIT_ENV).ok().as_deref())
}

pub(crate) fn idle_exit_window_for(raw: Option<&str>) -> Option<Duration> {
    match raw.map(str::trim) {
        None | Some("") => Some(DEFAULT_IDLE_EXIT),
        Some(value) => match value.parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(Duration::from_secs(secs)),
            // A malformed override keeps the default rather than silently
            // disabling the one thing that stops fixture brokers piling up.
            Err(_) => Some(DEFAULT_IDLE_EXIT),
        },
    }
}

/// Request shutdown once `is_idle` has held for `window`, sampling every
/// `interval`. Any busy sample restarts the clock, so a broker that is used
/// at least once per window never exits.
pub(crate) async fn run_idle_standdown(
    shutdown: Arc<ShutdownSignal>,
    interval: Duration,
    window: Duration,
    mut is_idle: impl FnMut() -> bool,
) {
    let mut idle_since: Option<Instant> = None;
    loop {
        tokio::select! {
            () = shutdown.wait() => return,
            _ = tokio::time::sleep(interval) => {}
        }
        if !is_idle() {
            idle_since = None;
            continue;
        }
        let since = *idle_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= window {
            eprintln!(
                "soldr broker: no route served and no connection held for {}s; standing down \
                 (soldr#3193, {IDLE_EXIT_ENV}=0 disables)",
                window.as_secs()
            );
            shutdown.request();
            return;
        }
    }
}

#[cfg(test)]
#[path = "broker_idle_tests.rs"]
mod tests;
