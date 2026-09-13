//! Stand a broker down once the home it was installed into is gone
//! (soldr#3075 leak 1, soldr#3057 findings 1-2).
//!
//! A broker lives at `<home>/.soldr/broker/soldr-broker`. Anything that runs
//! soldr under a disposable `HOME` (a pytest tmpdir, a clud tmp home) gets a
//! broker that calls `setsid`, outlives the run that created it, and is spared
//! by every process reaper as a session leader. Once that directory has been
//! deleted, no client can ever resolve this broker's endpoint again, and its
//! lease and bring-up logs have gone with it. It has nothing left to serve.
//!
//! The check watches the broker's install directory, not its executable. An
//! image replaced in place leaves the directory standing, so a live singleton
//! is never retired by one: soldr#2549 forbids stopping a broker for a version
//! or digest change. Only removal of the directory itself triggers the exit.
//!
//! Absence has to persist across [`DAEMON_IMAGE_MISSING_STRIKES`] consecutive
//! checks, using the same detector the daemon applies to its own image
//! (soldr#1987), so a transient `stat` failure cannot retire a broker.
//!
//! [`DAEMON_IMAGE_MISSING_STRIKES`]: crate::daemon::lifecycle::DAEMON_IMAGE_MISSING_STRIKES

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::daemon::shutdown_signal::ShutdownSignal;

/// How often the broker checks that its install directory still exists.
pub(crate) const BROKER_HOME_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Whether `executable`'s directory still exists. `None` when the path has no
/// parent to check (a bare file name has an empty one), which says nothing
/// about deletion and must not count as a strike.
pub(crate) fn install_directory_present(executable: &Path) -> Option<bool> {
    executable
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::is_dir)
}

/// Check `present` every `interval`; request shutdown once absence persists.
/// Returns when shutdown is requested, by this watch or by anything else.
pub(crate) async fn run_home_watch<F>(
    shutdown: Arc<ShutdownSignal>,
    interval: Duration,
    mut present: F,
) where
    F: FnMut() -> Option<bool>,
{
    let mut detector = crate::daemon::lifecycle::MissingImageDetector::default();
    loop {
        tokio::select! {
            () = shutdown.wait() => return,
            () = tokio::time::sleep(interval) => {}
        }
        if detector.observe(present()) {
            // Loud on purpose: a broker that exits on its own would otherwise
            // look like the crash it is not.
            eprintln!(
                "soldr broker: install directory no longer exists after {} checks; \
                 shutting down, nothing can reach this broker again (soldr#3075)",
                detector.strikes()
            );
            shutdown.request();
            return;
        }
    }
}

#[cfg(test)]
#[path = "broker_image_watch_tests.rs"]
mod tests;
