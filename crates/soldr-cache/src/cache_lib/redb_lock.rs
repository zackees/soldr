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
use std::collections::HashMap;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const OPEN_RETRY_TIMEOUT: Duration = Duration::from_secs(5);
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Ceiling for the exponential backoff (issue #2230).
///
/// A 5 s budget with a fixed 10 ms delay is ~500 attempts, essentially all of
/// which are guaranteed to fail: the holder is doing unbounded filesystem work,
/// not releasing between two consecutive polls. Doubling from 10 ms and capping
/// at 400 ms turns the same budget into ~13 attempts.
const OPEN_RETRY_MAX_DELAY: Duration = Duration::from_millis(400);

/// Consecutive budget exhaustions on one database path before the breaker
/// opens (issue #2230). The field evidence had a single pid log 235 consecutive
/// full-budget failures without ever adapting.
const BREAKER_THRESHOLD: u32 = 5;

/// How long the breaker stays open before the next opener is allowed to probe.
const BREAKER_COOLOFF: Duration = Duration::from_secs(5);

/// Minimum spacing between durable forensic records for one database path
/// (issue #2230). Exhaustions inside the window are counted and folded into the
/// next emitted record instead of each writing their own near-identical line.
const FORENSIC_SUMMARY_INTERVAL: Duration = Duration::from_secs(60);

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

thread_local! {
    /// Count of successful `state.redb` opens **on this thread**.
    ///
    /// Exists to make "this path acquires the database at most once"
    /// (soldr#2224) an assertable property rather than a code-reading
    /// exercise. Every acquisition is an exclusive whole-file lock with its
    /// own contention budget, so the count is the thing that matters.
    ///
    /// Per-thread, not process-wide, and that is the whole point: libtest
    /// runs cases concurrently in one process and plenty of them open the
    /// state DB, so a global counter would make any assertion on it a
    /// coin flip. The paths being measured are synchronous and single-
    /// threaded, so a thread-local reading is exact.
    static OPEN_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Snapshot of this thread's successful-open count. Take a reading before
/// and after a call to learn how many times it acquired the state database.
pub fn state_db_open_count() -> u64 {
    OPEN_COUNT.with(|count| count.get())
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
    open_state_db_with_retry(path, RetryPolicy::required(), OpenIntent::Required, || {})
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
        RetryPolicy::best_effort(),
        OpenIntent::BestEffort,
        || {},
    )
}

/// Everything the contended-open path is allowed to tune, in one injectable
/// bundle (issue #2230).
///
/// Injectable so the tests can exercise backoff shape, the circuit breaker, and
/// the forensic rate limiter in milliseconds instead of sleeping out real
/// multi-second budgets.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryPolicy {
    /// Total wall-clock budget for one open.
    pub timeout: Duration,
    /// First backoff sleep; doubles after every failed attempt.
    pub initial_delay: Duration,
    /// Ceiling for the doubling, so a long budget stays responsive.
    pub max_delay: Duration,
    /// Consecutive budget exhaustions on a path before the breaker opens.
    pub breaker_threshold: u32,
    /// How long an open breaker suppresses opens on that path.
    pub breaker_cooloff: Duration,
    /// Minimum spacing between durable forensic records for a path.
    pub summary_interval: Duration,
}

impl RetryPolicy {
    /// Budget for correctness-critical openers.
    pub(crate) fn required() -> Self {
        Self {
            timeout: OPEN_RETRY_TIMEOUT,
            initial_delay: OPEN_RETRY_DELAY,
            max_delay: OPEN_RETRY_MAX_DELAY,
            breaker_threshold: BREAKER_THRESHOLD,
            breaker_cooloff: BREAKER_COOLOFF,
            summary_interval: FORENSIC_SUMMARY_INTERVAL,
        }
    }

    /// Budget for losable bookkeeping writes on a latency-critical path.
    pub(crate) fn best_effort() -> Self {
        Self {
            timeout: BEST_EFFORT_OPEN_TIMEOUT,
            ..Self::required()
        }
    }
}

/// Backoff sleep for `attempt` (1-based), exponential and jittered.
///
/// Full-jitter: the delay is drawn from `[base/2, base)` where `base` doubles
/// per attempt up to `max_delay`. The halving keeps the total sequence inside
/// the same budget while de-synchronising N contenders — a fixed delay makes
/// every waiter poll in lockstep, so they all miss the same release window and
/// then all collide on the next one.
fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let base_ns = policy
        .initial_delay
        .as_nanos()
        .saturating_mul(1u128 << attempt.saturating_sub(1).min(32))
        .min(policy.max_delay.as_nanos())
        .max(1);
    let half = (base_ns / 2).max(1);
    let jitter = u128::from(next_jitter()) % half.max(1);
    Duration::from_nanos((half + jitter).min(u128::from(u64::MAX)) as u64)
}

/// Cheap per-process pseudo-random source for the backoff jitter.
///
/// Deliberately not `rand`: this file has no such dependency and a jittered
/// sleep does not justify adding one. An xorshift seeded from the pid and the
/// process's own clock is enough to de-synchronise contenders both within a
/// process (the counter advances per call) and across processes (the seed
/// differs per pid).
fn next_jitter() -> u64 {
    static STATE: OnceLock<AtomicU64> = OnceLock::new();
    let state = STATE.get_or_init(|| {
        let pid = u64::from(std::process::id());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        AtomicU64::new(pid.rotate_left(32) ^ nanos | 1)
    });
    // xorshift64* on a shared counter: relaxed is fine, we only need distinct
    // values, not a total order.
    let mut x = state.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x
}

fn open_state_db_with_retry(
    path: &Path,
    policy: RetryPolicy,
    intent: OpenIntent,
    mut on_contention: impl FnMut(),
) -> Result<StateDbHandle, redb::DatabaseError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Checked *before* the process-wide mutex: the whole point of the breaker
    // is to not queue behind an opener that is about to burn its budget.
    if breaker_should_fail_fast(path, &policy) {
        return Err(redb::DatabaseError::DatabaseAlreadyOpen);
    }
    let guard = state_db_open_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Another caller may have exhausted its budget and opened the breaker
    // while this caller waited for the in-process mutex. Re-check before the
    // retry loop so already-queued callers do not each pay a full budget.
    if breaker_should_fail_fast(path, &policy) {
        return Err(redb::DatabaseError::DatabaseAlreadyOpen);
    }
    let started = Instant::now();
    let mut attempts: u32 = 0;
    loop {
        match Database::builder().create(path) {
            Ok(db) => {
                OPEN_COUNT.with(|count| count.set(count.get().saturating_add(1)));
                note_open_succeeded(path);
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
            Err(redb::DatabaseError::DatabaseAlreadyOpen) if started.elapsed() < policy.timeout => {
                attempts += 1;
                on_contention();
                // Never overshoot the budget: clamp the sleep to what is left
                // so the caller's contract ("gives up after `timeout`") holds
                // even at the 400 ms ceiling.
                let remaining = policy.timeout.saturating_sub(started.elapsed());
                std::thread::sleep(backoff_delay(&policy, attempts).min(remaining));
            }
            Err(error) => {
                if matches!(error, redb::DatabaseError::DatabaseAlreadyOpen) {
                    note_open_exhausted(path, &policy);
                    report_contention(path, intent, attempts, started.elapsed(), true, &policy);
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
    policy: &RetryPolicy,
) {
    warn_contention(path, intent, attempts, elapsed, exhausted);
    // Issue #2230: the field evidence was 1,347 near-identical `budget-
    // exhausted` lines. One line per failure is not a signal anyone reads, so
    // the durable half is rate limited and the suppressed failures are folded
    // into the next record instead.
    if let Some(summary) = forensics_admit(path, attempts, elapsed, policy) {
        append_contention_record(
            path,
            intent,
            attempts,
            elapsed.as_millis(),
            exhausted,
            &summary,
        );
    }
}

/// Rolled-up view of the exhaustions a record covers (issue #2230).
#[derive(Debug, Clone, Copy, Default)]
struct ContentionSummary {
    /// Exhaustions on this path suppressed since the last emitted record.
    suppressed: u64,
    /// Length of the window those suppressed records fell into.
    window_ms: u128,
    /// Worst attempt count seen in the window (this record's included).
    worst_attempts: u32,
    /// Worst wall-clock burned in the window (this record's included).
    worst_elapsed_ms: u128,
}

/// Per-database-path bookkeeping shared by the breaker and the rate limiter.
#[derive(Debug, Default)]
struct PathState {
    /// Consecutive budget exhaustions with no intervening success.
    consecutive_exhaustions: u32,
    /// When set, the breaker is open until this instant.
    open_until: Option<Instant>,
    /// When the last durable forensic record was written.
    last_emit: Option<Instant>,
    /// Exhaustions suppressed since `last_emit`.
    suppressed: u64,
    worst_attempts: u32,
    worst_elapsed_ms: u128,
}

fn path_states() -> &'static Mutex<HashMap<PathBuf, PathState>> {
    static STATES: OnceLock<Mutex<HashMap<PathBuf, PathState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_path_state<R>(path: &Path, f: impl FnOnce(&mut PathState) -> R) -> R {
    let mut states = path_states()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(states.entry(path.to_path_buf()).or_default())
}

/// Circuit-breaker gate (issue #2230).
///
/// Returns `true` when the breaker is open and the caller must fail fast
/// instead of paying the full budget. Also performs the *close* transition:
/// once the cooloff elapses the next opener probes for real, and that is where
/// the single "closed" lifecycle event is emitted.
fn breaker_should_fail_fast(path: &Path, policy: &RetryPolicy) -> bool {
    if policy.breaker_threshold == 0 {
        return false;
    }
    enum Transition {
        Suppress,
        Closed,
        Pass,
    }
    let transition = with_path_state(path, |state| match state.open_until {
        Some(until) if Instant::now() < until => Transition::Suppress,
        Some(_) => {
            state.open_until = None;
            state.consecutive_exhaustions = 0;
            Transition::Closed
        }
        None => Transition::Pass,
    });
    match transition {
        Transition::Suppress => true,
        Transition::Closed => {
            breaker_lifecycle(path, "state_db_open_breaker_closed", policy, |db| {
                tracing::warn!(
                    event = "state_db_open_breaker_closed",
                    db = %db,
                    "state.redb open breaker cooloff elapsed; probing again (issue #2230)",
                );
            });
            false
        }
        Transition::Pass => false,
    }
}

/// A successful open is the breaker's reset: the contention cleared.
fn note_open_succeeded(path: &Path) {
    with_path_state(path, |state| {
        state.consecutive_exhaustions = 0;
    });
}

/// Count one budget exhaustion and open the breaker at the threshold.
fn note_open_exhausted(path: &Path, policy: &RetryPolicy) {
    if policy.breaker_threshold == 0 {
        return;
    }
    let opened = with_path_state(path, |state| {
        state.consecutive_exhaustions = state.consecutive_exhaustions.saturating_add(1);
        if state.consecutive_exhaustions >= policy.breaker_threshold && state.open_until.is_none() {
            state.open_until = Some(Instant::now() + policy.breaker_cooloff);
            return Some(state.consecutive_exhaustions);
        }
        None
    });
    let Some(consecutive) = opened else {
        return;
    };
    let cooloff_ms = policy.breaker_cooloff.as_millis();
    breaker_lifecycle(path, "state_db_open_breaker_opened", policy, move |db| {
        tracing::warn!(
            event = "state_db_open_breaker_opened",
            consecutive,
            cooloff_ms,
            db = %db,
            "state.redb open budget was exhausted {consecutive} times in a row; \
             failing opens fast for {cooloff_ms}ms instead of stalling every \
             caller for the full budget (issue #2230)",
        );
    });
}

/// Emit the loud + durable pair for one breaker transition.
///
/// Exactly two of these fire per contention episode — one on open, one on
/// close — so unlike the per-failure record they are never rate limited.
fn breaker_lifecycle(
    path: &Path,
    event: &str,
    policy: &RetryPolicy,
    warn: impl FnOnce(std::path::Display<'_>),
) {
    warn(path.display());
    append_json_record(
        path,
        serde_json::json!({
            "ts_ms": unix_ms(),
            "pid": std::process::id(),
            "event": event,
            "cooloff_ms": policy.breaker_cooloff.as_millis(),
            "db": path.display().to_string(),
        }),
    );
}

/// Rate limiter for the durable half (issue #2230).
///
/// Returns `Some(summary)` when this exhaustion is allowed to write a record —
/// the first one on a path, and then at most one per `summary_interval`. The
/// summary carries the failures suppressed since the last emission so no
/// contention is silently lost, only compressed.
fn forensics_admit(
    path: &Path,
    attempts: u32,
    elapsed: Duration,
    policy: &RetryPolicy,
) -> Option<ContentionSummary> {
    let elapsed_ms = elapsed.as_millis();
    with_path_state(path, |state| {
        let now = Instant::now();
        state.worst_attempts = state.worst_attempts.max(attempts);
        state.worst_elapsed_ms = state.worst_elapsed_ms.max(elapsed_ms);
        let due = match state.last_emit {
            None => true,
            Some(last) => now.duration_since(last) >= policy.summary_interval,
        };
        if !due {
            state.suppressed = state.suppressed.saturating_add(1);
            return None;
        }
        let window_ms = state
            .last_emit
            .map(|last| now.duration_since(last).as_millis())
            .unwrap_or(0);
        let summary = ContentionSummary {
            suppressed: state.suppressed,
            window_ms,
            worst_attempts: state.worst_attempts,
            worst_elapsed_ms: state.worst_elapsed_ms,
        };
        state.last_emit = Some(now);
        state.suppressed = 0;
        state.worst_attempts = 0;
        state.worst_elapsed_ms = 0;
        Some(summary)
    })
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
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
    summary: &ContentionSummary,
) {
    append_json_record(
        path,
        serde_json::json!({
            "ts_ms": unix_ms(),
            "pid": std::process::id(),
            "event": if exhausted { "budget-exhausted" } else { "contended" },
            "intent": intent.as_str(),
            "attempts": attempts,
            "elapsed_ms": elapsed_ms,
            // Issue #2230 aggregation: how many identical failures this one
            // line stands in for, and the worst case among them.
            "suppressed": summary.suppressed,
            "window_ms": summary.window_ms,
            "worst_attempts": summary.worst_attempts,
            "worst_elapsed_ms": summary.worst_elapsed_ms,
            "db": path.display().to_string(),
        }),
    );
}

/// Append one JSONL line, swallowing every failure. Shared by the per-open
/// records and the breaker lifecycle events.
fn append_json_record(path: &Path, record: serde_json::Value) {
    use std::io::Write;

    let Some(root) = path.parent() else {
        return;
    };
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
#[path = "redb_lock_tests.rs"]
mod tests;
