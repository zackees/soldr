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

/// Budget for best-effort openers on a latency-critical path
/// ([`open_state_db_best_effort`], issue #1814).
///
/// Deliberately ~100× shorter than [`OPEN_RETRY_TIMEOUT`]: the callers that
/// use it are writing GC bookkeeping (a `target/` last-used timestamp), so
/// losing one write costs nothing but stalling a rustc invocation for 5 s
/// costs the whole build.
const BEST_EFFORT_OPEN_TIMEOUT: Duration = Duration::from_millis(50);

/// Durable forensic record of state-DB lock contention, next to the other
/// `~/.soldr/logs/*.jsonl` records.
const CONTENTION_LOG_FILE: &str = "redb-contention.jsonl";

/// Why the state DB was opened, for the contention forensics. Contention on a
/// best-effort open is a skipped write; on a required open it is a stall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenIntent {
    /// Correctness-critical: the caller cannot proceed without the DB, so it
    /// waits out the full [`OPEN_RETRY_TIMEOUT`].
    Required,
    /// Best-effort bookkeeping: the caller would rather skip the write than
    /// block. See [`BEST_EFFORT_OPEN_TIMEOUT`].
    BestEffort,
}

impl OpenIntent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::BestEffort => "best_effort",
        }
    }
}

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
    open_state_db_with_retry(
        path,
        OPEN_RETRY_TIMEOUT,
        OPEN_RETRY_DELAY,
        OpenIntent::Required,
        || {},
    )
}

/// Open the shared state database for a latency-critical, losable write
/// (issue #1814).
///
/// Identical to [`open_state_db`] except the cross-process retry budget is
/// [`BEST_EFFORT_OPEN_TIMEOUT`] instead of 5 s. Contention returns
/// `DatabaseAlreadyOpen` promptly so the caller can skip its write rather
/// than block a rustc invocation behind another process's redb handle.
///
/// **Never use this for a write another component will later read as
/// authoritative.** It exists for the wrapper's per-invocation `target/`
/// registry touch, where the row is GC bookkeeping and the next invocation
/// re-touches it anyway.
pub fn open_state_db_best_effort(path: &Path) -> Result<StateDbHandle, redb::DatabaseError> {
    open_state_db_with_retry(
        path,
        BEST_EFFORT_OPEN_TIMEOUT,
        OPEN_RETRY_DELAY,
        OpenIntent::BestEffort,
        || {},
    )
}

fn open_state_db_with_retry(
    path: &Path,
    timeout: Duration,
    retry_delay: Duration,
    intent: OpenIntent,
    mut on_contention: impl FnMut(),
) -> Result<StateDbHandle, redb::DatabaseError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let guard = state_db_open_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let started = Instant::now();
    let mut attempts: u32 = 0;
    loop {
        match Database::builder().create(path) {
            Ok(db) => {
                // Issue #1814: the retry loop (#1655) is a rare cold path, not
                // a routine one. Staying silent when it fires is what let
                // multi-process contention masquerade as an unexplained stall,
                // so a *resolved* wait is still reported with its cost.
                //
                // Tracing only — no filesystem I/O on this path. We are still
                // inside the `state_db_open_lock` critical section, and adding
                // a `create_dir_all` + append here serializes every contended
                // opener behind it. That regressed `build_log`'s 10 s test
                // budget, which spends two 5 s `Required` opens back to back.
                // A resolved wait also has no failure to make durable: it
                // succeeded, and the warn already carries the forensics.
                if attempts > 0 {
                    warn_contention(path, intent, attempts, started.elapsed(), false);
                }
                return Ok(StateDbHandle::new(db, guard));
            }
            Err(redb::DatabaseError::DatabaseAlreadyOpen) if started.elapsed() < timeout => {
                attempts += 1;
                on_contention();
                std::thread::sleep(retry_delay);
            }
            Err(error) => {
                if matches!(error, redb::DatabaseError::DatabaseAlreadyOpen) {
                    report_contention(path, intent, attempts, started.elapsed(), true);
                }
                return Err(error);
            }
        }
    }
}

/// Emit the loud **and** durable pair for a contention event that ended in
/// failure.
///
/// Both halves are mandatory per the repo's loud-forensics rule when a budget
/// actually fires: the `tracing` line reaches an attached operator, and the
/// JSONL record survives a detached wrapper process whose stderr went nowhere.
///
/// Only the exhausted path takes the durable half — see the `Ok` arm of
/// [`open_state_db_with_retry`] for why a resolved wait must stay I/O-free.
fn report_contention(
    path: &Path,
    intent: OpenIntent,
    attempts: u32,
    elapsed: Duration,
    exhausted: bool,
) {
    warn_contention(path, intent, attempts, elapsed, exhausted);
    append_contention_record(path, intent, attempts, elapsed.as_millis(), exhausted);
}

/// The loud half on its own: a `tracing` event with the full forensic detail
/// and no syscalls, so it is safe to call inside the open critical section.
fn warn_contention(
    path: &Path,
    intent: OpenIntent,
    attempts: u32,
    elapsed: Duration,
    exhausted: bool,
) {
    let elapsed_ms = elapsed.as_millis();
    let db = path.display();
    if exhausted {
        tracing::warn!(
            event = "state_db_lock_budget_exhausted",
            intent = intent.as_str(),
            attempts,
            elapsed_ms,
            db = %db,
            "state.redb is held by another process and the open budget ran out \
             after {attempts} attempts / {elapsed_ms}ms (intent={}); see issue #1814",
            intent.as_str(),
        );
    } else {
        tracing::warn!(
            event = "state_db_lock_contended",
            intent = intent.as_str(),
            attempts,
            elapsed_ms,
            db = %db,
            "state.redb open waited {elapsed_ms}ms across {attempts} retries for \
             another process to release it (intent={}); see issue #1814",
            intent.as_str(),
        );
    }
}

/// Append one JSONL line to `<root>/logs/redb-contention.jsonl`.
///
/// The state DB lives at `<root>/state.redb`, so the root is the DB's parent —
/// this layer only ever receives the DB path, never a `SoldrPaths`.
/// Best-effort by construction: a diagnostic that fails must never turn into a
/// build failure.
fn append_contention_record(
    path: &Path,
    intent: OpenIntent,
    attempts: u32,
    elapsed_ms: u128,
    exhausted: bool,
) {
    use std::io::Write;

    let Some(root) = path.parent() else {
        return;
    };
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let record = serde_json::json!({
        "ts_ms": ts_ms,
        "pid": std::process::id(),
        "event": if exhausted { "budget-exhausted" } else { "contended" },
        "intent": intent.as_str(),
        "attempts": attempts,
        "elapsed_ms": elapsed_ms,
        "db": path.display().to_string(),
    });
    let Ok(line) = serde_json::to_string(&record) else {
        return;
    };
    let dir = root.join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(CONTENTION_LOG_FILE))
    {
        let _ = writeln!(file, "{line}");
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
    // Plain libtest runs these cases concurrently inside one process, where
    // they intentionally share the production state-db mutex. Nextest gives
    // each case its own process and tempdir, so cross-test serialization is
    // unnecessary there.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn serial_test_guard() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    crate::timed_test!(subprocess_lock_holder, {
        let Some(dir) = std::env::var_os(LOCK_HOLDER_DIR_ENV).map(PathBuf::from) else {
            return;
        };
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
            OpenIntent::Required,
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
        // The worker's retry budget must comfortably exceed
        // OBSERVE_CONTENTION_WAIT below. The blocker is released only
        // *after* the parent observes contention, so a budget equal to
        // that wait lets the worker give up before the release it is
        // waiting for ever happens. Both were 1s, which raced on the
        // emulated aarch64-Windows lane: the parent spent most of its
        // second in `recv_timeout`, and by the time it dropped the
        // blocker the worker had already returned `Err`.
        //
        // A large budget is free on the happy path — the worker returns
        // as soon as the open succeeds, not when the budget expires.
        const WORKER_RETRY_BUDGET: Duration = Duration::from_secs(30);
        // Generous for the same reason: the emulated Windows lanes are
        // an order of magnitude slower than the host ones, and this
        // asserts *that* contention is observed, not how fast.
        const OBSERVE_CONTENTION_WAIT: Duration = Duration::from_secs(10);

        let worker = std::thread::spawn(move || {
            open_state_db_with_retry(
                &worker_path,
                WORKER_RETRY_BUDGET,
                Duration::from_millis(5),
                OpenIntent::Required,
                || {
                    let _ = contended_tx.try_send(());
                },
            )
            .map(drop)
        });

        contended_rx
            .recv_timeout(OBSERVE_CONTENTION_WAIT)
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
        let result = open_state_db_with_retry(
            &path,
            budget,
            Duration::from_millis(5),
            OpenIntent::Required,
            || {
                observed.fetch_add(1, Ordering::Relaxed);
            },
        );
        let error = match result {
            Ok(_) => panic!("contention should outlive the retry budget"),
            Err(error) => error,
        };

        assert!(matches!(error, redb::DatabaseError::DatabaseAlreadyOpen));
        assert!(attempts.load(Ordering::Relaxed) >= 2);
        assert!(started.elapsed() >= budget);
        assert!(started.elapsed() < Duration::from_secs(3));
    });

    // Issue #1814: a contended open must leave a durable record even when
    // nobody is watching stderr. This is the forensic half of the loud-plus-
    // durable pair; the tracing half is not capturable here.
    crate::timed_test!(contention_appends_a_durable_forensic_record, {
        let _test_guard = serial_test_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.redb");
        let _blocker = Database::builder().create(&path).expect("blocking open");

        let result = open_state_db_with_retry(
            &path,
            Duration::from_millis(80),
            Duration::from_millis(5),
            OpenIntent::BestEffort,
            || {},
        );
        assert!(result.is_err(), "blocked open must not succeed");

        let log = fs::read_to_string(dir.path().join("logs").join(CONTENTION_LOG_FILE))
            .expect("contention log must exist after a contended open");
        let line = log.lines().next().expect("at least one record");
        assert!(line.contains(r#""event":"budget-exhausted""#), "{line}");
        assert!(line.contains(r#""intent":"best_effort""#), "{line}");
        assert!(line.contains(r#""attempts":"#), "{line}");
        assert!(line.contains(r#""elapsed_ms":"#), "{line}");
        assert!(
            line.contains(&format!(r#""pid":{}"#, std::process::id())),
            "{line}"
        );
    });

    // Issue #1814 acceptance: no 5 s stalls on the wrapper hot path. The
    // best-effort budget must give up in tens of milliseconds while the
    // required budget keeps waiting out the full window.
    crate::timed_test!(best_effort_open_gives_up_far_sooner_than_required, {
        let _test_guard = serial_test_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.redb");
        let _blocker = Database::builder().create(&path).expect("blocking open");

        let started = Instant::now();
        let result = open_state_db_best_effort(&path);
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(redb::DatabaseError::DatabaseAlreadyOpen)),
            "a held database must surface as contention, not another error"
        );
        assert!(
            elapsed >= BEST_EFFORT_OPEN_TIMEOUT,
            "best-effort open must still honor its own budget, took {elapsed:?}"
        );
        assert!(
            elapsed < OPEN_RETRY_TIMEOUT / 2,
            "best-effort open must not inherit the {OPEN_RETRY_TIMEOUT:?} 
             required budget; took {elapsed:?} (issue #1814)"
        );
    });

    // A wait that RESOLVED must not touch the filesystem. `report_contention`
    // runs inside the `state_db_open_lock` critical section, so an append here
    // serializes every contended opener behind a `create_dir_all` + write —
    // which blew `build_log`'s 10 s test budget (two 5 s `Required` opens back
    // to back) on the musl lane. The loud half still fires via tracing.
    crate::timed_test!(resolved_contention_stays_io_free, {
        let _test_guard = serial_test_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.redb");
        let blocker = Database::builder().create(&path).expect("blocking open");

        // Release the blocker on the first observed contention so the open
        // resolves after at least one retry.
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&released);
        let mut blocker = Some(blocker);
        let opened = open_state_db_with_retry(
            &path,
            Duration::from_secs(5),
            Duration::from_millis(5),
            OpenIntent::Required,
            || {
                if !flag.swap(true, Ordering::Relaxed) {
                    drop(blocker.take());
                }
            },
        );
        drop(opened.expect("open resolves once the blocker releases"));

        assert!(
            released.load(Ordering::Relaxed),
            "the test must actually have gone through the retry path"
        );
        assert!(
            !dir.path().join("logs").join(CONTENTION_LOG_FILE).exists(),
            "a resolved wait must not write a durable record from inside the \
             open critical section"
        );
    });

    // A clean open must not write a contention record - otherwise the log
    // becomes noise and stops being evidence of a real problem.
    crate::timed_test!(uncontended_open_writes_no_forensic_record, {
        let _test_guard = serial_test_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.redb");

        let handle = open_state_db(&path).expect("uncontended open");
        drop(handle);

        assert!(
            !dir.path().join("logs").join(CONTENTION_LOG_FILE).exists(),
            "an uncontended open must leave no contention record"
        );
    });
}
