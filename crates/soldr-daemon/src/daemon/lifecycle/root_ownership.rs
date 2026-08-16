//! Version-independent root ownership: the file lock one daemon holds for
//! its whole lifetime, plus the stop-then-relaunch acquisition grace window.
//! Split out of `lifecycle/mod.rs` for the per-file LOC ratchet.

use crate::cache_lib::soldr_daemon_dir;
use crate::core::SoldrPaths;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::time::{Duration, Instant};

const ROOT_OWNER_LOCK_NAME: &str = "root-owner.lock";

/// Version-independent ownership for one product root. The daemon holds this
/// for its whole lifetime; explicit orphan-root maintenance uses the same lock
/// so startup and manual deletion cannot race even across protocol versions.
pub struct RootOwnershipGuard {
    file: File,
}

impl RootOwnershipGuard {
    pub fn try_acquire(paths: &SoldrPaths) -> std::io::Result<Option<Self>> {
        let dir = soldr_daemon_dir(paths);
        fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join(ROOT_OWNER_LOCK_NAME))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { file })),
            Err(error) if crate::cache_lib::cargo_lock::lock_is_held(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// [`Self::try_acquire`] with a bounded grace window for a lock still
    /// held by an exiting daemon.
    ///
    /// `soldr daemon stop` acknowledges before the stopped daemon's process
    /// fully exits, and the root-owner lock is only released when its handle
    /// closes at exit. A daemon launched into that window — the broker
    /// relaunches on the very next compile — used to fail its single
    /// acquisition attempt and die with "root ownership is busy", failing the
    /// whole build (observed as the Windows
    /// `cargo_test_recovers_after_daemon_stop_without_herd_spawning` CI
    /// failure).
    ///
    /// The retry is deliberately conditional: `is_serving` reports whether a
    /// live daemon currently serves this root. While one does, the busy lock
    /// means a healthy owner and the caller gets `AlreadyServing` immediately
    /// — a redundant spawn must keep backing off fast (issue #1814). Only a
    /// busy lock with *nobody serving* is the exit-in-progress window worth
    /// waiting out.
    pub fn acquire_with_grace(
        paths: &SoldrPaths,
        budget: Duration,
        poll: Duration,
        mut is_serving: impl FnMut() -> Option<u32>,
    ) -> std::io::Result<RootAcquireOutcome> {
        let deadline = Instant::now() + budget;
        let mut waited = false;
        loop {
            if let Some(guard) = Self::try_acquire(paths)? {
                return Ok(RootAcquireOutcome::Acquired(guard));
            }
            if let Some(pid) = is_serving() {
                return Ok(RootAcquireOutcome::AlreadyServing(pid));
            }
            if Instant::now() >= deadline {
                return Ok(RootAcquireOutcome::TimedOut);
            }
            if !waited {
                waited = true;
                tracing::info!(
                    "root ownership is busy with no daemon serving; waiting up to {budget:?} \
                     for the previous owner to finish exiting"
                );
            }
            std::thread::sleep(poll);
        }
    }
}

/// Result of [`RootOwnershipGuard::acquire_with_grace`].
pub enum RootAcquireOutcome {
    /// This process now owns the root.
    Acquired(RootOwnershipGuard),
    /// A live daemon serves this root; the caller is a redundant spawn.
    AlreadyServing(u32),
    /// The lock stayed busy with nobody serving for the whole budget.
    TimedOut,
}

impl Drop for RootOwnershipGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}
