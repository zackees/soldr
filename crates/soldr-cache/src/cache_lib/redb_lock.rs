//! Process-wide serialization for opening the soldr `state.redb` file.
//!
//! redb refuses concurrent in-process `Database::open` against the same
//! file with `Database already open. Cannot acquire lock.`. The daemon's
//! per-request handlers each call `open_db` on `state.redb`, so two
//! handlers landing on different tokio worker threads can race and one
//! of them silently fails — see issue #608 for the regression this
//! mutex fixes (a daemon-side `db` write silently dropped its row when a
//! concurrent `Status` request had the same file open via
//! `cook_index::stats`).
//!
//! The lock is held for the full lifetime of the redb [`Database`]
//! handle, not just the open call: the file lock redb holds is released
//! when the `Database` is dropped, so callers must keep this guard alive
//! at least that long. The [`StateDbHandle`] wrapper enforces drop order
//! by declaring `db` before `_guard`.

use redb::Database;
use std::ops::Deref;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const OPEN_RETRY_TIMEOUT: Duration = Duration::from_secs(5);
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Shared global lock guarding every in-process `Database::open` on
/// `state.redb`. Both `daemon::db` and `cache_lib::cook_index` go
/// through this lock — they open the same file.
pub fn state_db_open_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Open the shared state database, waiting briefly for another soldr process
/// to release redb's exclusive file-open lock.
///
/// The process-wide mutex prevents in-process overlap. A separate CLI and
/// daemon can still race at process boundaries, where redb reports
/// [`redb::DatabaseError::DatabaseAlreadyOpen`] instead of waiting. Retry only
/// that transient variant; corruption, upgrade, and I/O errors remain
/// immediate failures.
pub fn open_state_db(path: &Path) -> Result<StateDbHandle, redb::DatabaseError> {
    open_state_db_with_retry(path, OPEN_RETRY_TIMEOUT, OPEN_RETRY_DELAY, || {})
}

fn open_state_db_with_retry(
    path: &Path,
    timeout: Duration,
    retry_delay: Duration,
    mut on_contention: impl FnMut(),
) -> Result<StateDbHandle, redb::DatabaseError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let guard = state_db_open_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let started = Instant::now();
    loop {
        match Database::builder().create(path) {
            Ok(db) => return Ok(StateDbHandle::new(db, guard)),
            Err(redb::DatabaseError::DatabaseAlreadyOpen) if started.elapsed() < timeout => {
                on_contention();
                std::thread::sleep(retry_delay);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Owns a redb [`Database`] handle together with the
/// [`state_db_open_lock`] guard that gates concurrent opens. Field
/// order is load-bearing: `db` is declared first so it is dropped
/// (releasing redb's file lock) before `_guard` is dropped (letting
/// the next opener proceed).
pub struct StateDbHandle {
    db: Database,
    _guard: MutexGuard<'static, ()>,
}

impl StateDbHandle {
    pub fn new(db: Database, guard: MutexGuard<'static, ()>) -> Self {
        Self { db, _guard: guard }
    }
}

impl Deref for StateDbHandle {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    const LOCK_HOLDER_DIR_ENV: &str = "SOLDR_REDB_LOCK_HOLDER_DIR";

    fn serial_test_guard() -> MutexGuard<'static, ()> {
        static TEST_LOCK: Mutex<()> = Mutex::new(());
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    crate::timed_test!(subprocess_lock_holder, {
        let Some(dir) = std::env::var_os(LOCK_HOLDER_DIR_ENV).map(PathBuf::from) else {
            return;
        };
        let _test_guard = serial_test_guard();
        let db = Database::builder()
            .create(dir.join("state.redb"))
            .expect("subprocess blocking open");
        fs::write(dir.join("ready"), b"").expect("write ready marker");

        let release = dir.join("release");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !release.exists() {
            assert!(
                Instant::now() < deadline,
                "parent did not release subprocess lock holder"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(db);
    });

    crate::timed_test!(retries_actual_subprocess_contention_until_release, {
        let _test_guard = serial_test_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let test_name = "cache_lib::redb_lock::tests::subprocess_lock_holder";
        let mut child = Command::new(std::env::current_exe().expect("current test executable"))
            .args(["--exact", test_name, "--nocapture"])
            .env(LOCK_HOLDER_DIR_ENV, dir.path())
            .spawn()
            .expect("spawn lock-holder subprocess");

        let ready = dir.path().join("ready");
        let ready_deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                child.try_wait().expect("poll lock-holder child").is_none(),
                "lock-holder subprocess exited before acquiring the database"
            );
            assert!(
                Instant::now() < ready_deadline,
                "lock-holder subprocess did not acquire the database"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        let mut observed_contention = false;
        let opened = open_state_db_with_retry(
            &dir.path().join("state.redb"),
            Duration::from_secs(5),
            Duration::from_millis(5),
            || {
                if !observed_contention {
                    observed_contention = true;
                    fs::write(dir.path().join("release"), b"").expect("release lock holder");
                }
            },
        );

        opened.expect("open succeeded after subprocess released database");
        assert!(
            observed_contention,
            "parent must observe subprocess contention"
        );
        assert!(
            child.wait().expect("wait for lock-holder child").success(),
            "lock-holder subprocess failed"
        );
    });

    crate::timed_test!(retries_cross_process_style_open_contention_until_release, {
        let _test_guard = serial_test_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.redb");
        // Bypass the soldr mutex to model a different process holding redb's
        // file lock.
        let blocker = Database::builder().create(&path).expect("blocking open");
        let (contended_tx, contended_rx) = mpsc::sync_channel(1);
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            open_state_db_with_retry(
                &worker_path,
                Duration::from_secs(1),
                Duration::from_millis(5),
                || {
                    let _ = contended_tx.try_send(());
                },
            )
            .map(drop)
        });

        contended_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker observed contention");
        drop(blocker);

        worker
            .join()
            .expect("worker joined")
            .expect("open succeeded after release");
    });

    crate::timed_test!(stops_retrying_after_injected_budget, {
        let _test_guard = serial_test_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.redb");
        let _blocker = Database::builder().create(&path).expect("blocking open");
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);

        let budget = Duration::from_secs(1);
        let started = Instant::now();
        let result = open_state_db_with_retry(&path, budget, Duration::from_millis(5), || {
            observed.fetch_add(1, Ordering::Relaxed);
        });
        let error = match result {
            Ok(_) => panic!("contention should outlive the retry budget"),
            Err(error) => error,
        };

        assert!(matches!(error, redb::DatabaseError::DatabaseAlreadyOpen));
        assert!(attempts.load(Ordering::Relaxed) >= 2);
        assert!(started.elapsed() >= budget);
        assert!(started.elapsed() < Duration::from_secs(3));
    });
}
