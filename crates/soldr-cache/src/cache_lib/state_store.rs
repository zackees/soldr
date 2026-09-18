//! SQLite-backed shared state store for soldr (`~/.soldr/state.sqlite3`).
//!
//! Replaces the redb store (and the whole `redb_lock` retry/breaker
//! apparatus that compensated for it). redb's `Database::open` takes an
//! exclusive whole-file lock, so every concurrent reader — a `soldr gc
//! list` racing the daemon, two IPC handlers on different tokio workers,
//! the per-compile wrapper touch racing either — was a lock-contention
//! incident that needed retries, backoff, circuit breakers, forensics
//! logs, and daemon-mediated read routing to paper over. SQLite in WAL
//! mode gives concurrent readers alongside a single writer natively, and
//! `busy_timeout` makes writer-vs-writer contention a bounded wait inside
//! the library instead of a failed open:
//!
//! * Readers never block writers and writers never block readers (WAL).
//! * A second writer waits up to the busy timeout, then errors — no
//!   silent drop, no per-caller retry loop.
//! * Handles are cheap per-operation connections; there is no process-wide
//!   open mutex and no cross-process "database already open" failure mode.
//!
//! Every table in the store is created here at open, so no read path ever
//! needs a write transaction to guarantee a table exists (the soldr#2224
//! concern the old module handled with per-module `read_table` dances).
//!
//! ## Every transaction on this store is `BEGIN IMMEDIATE` (soldr#3288/#3290)
//!
//! WAL mode permits exactly one writer at a time. rusqlite's default
//! `unchecked_transaction()` opens with `BEGIN DEFERRED`, which takes no
//! lock at all: a transaction that first reads (pinning a WAL snapshot)
//! and later writes has to *upgrade* mid-transaction, and that upgrade can
//! fail with `SQLITE_BUSY_SNAPSHOT` (extended code 517) the instant another
//! connection commits in between. Critically, SQLite does **not** invoke
//! the busy handler for that upgrade — `busy_timeout` is bypassed entirely,
//! not exceeded — so the failure is immediate no matter how generous the
//! timeout is, and rusqlite renders it as the same "database is locked"
//! text as a real timeout. That is soldr#3288: an intra-process race
//! between two connections in `soldr-daemon`, not a slow lock holder.
//!
//! [`open_with`] fixes this centrally by calling
//! `set_transaction_behavior(TransactionBehavior::Immediate)` on every
//! connection this module opens. `Connection::unchecked_transaction`
//! reads that setting, so every existing (and future) `unchecked_transaction()`
//! call against a handle from this store takes the write lock at `BEGIN`,
//! where `busy_timeout` *does* apply — turning an unavoidable instant
//! failure into a bounded wait. `StateDbHandle` only implements `Deref`
//! (no `DerefMut`), so a caller cannot reach back into the `Connection` and
//! flip the behavior back to deferred.
//!
//! Trade-off, stated plainly: writers that used to interleave (a deferred
//! reader-then-writer could run alongside another writer right up to the
//! upgrade) now serialize from `BEGIN`. That is acceptable here because
//! every transaction on this store is short and holds no I/O, network
//! call, or subprocess — see `write_batch` in `event_batcher.rs` and
//! `unit_memory_history.rs` for the two representative shapes.
//!
//! ## Legacy `state.redb`
//!
//! A sibling `state.redb` written by pre-SQLite soldr is deleted on first
//! open. All of its contents are disposable local bookkeeping (target
//! recency rows, build history snapshots, cook index rows — the on-disk
//! cook artifacts are unaffected and re-index on the next `soldr cook`),
//! matching the precedent of the #580/#603 row-format migrations.

use rusqlite::{Connection, TransactionBehavior};
use std::ops::Deref;
use std::path::Path;
use std::time::Duration;

/// Busy timeout for correctness-critical openers: a writer waits up to
/// this long for a concurrent writer's transaction to finish.
const REQUIRED_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Busy timeout for latency-critical, losable writes (issue #1814): the
/// wrapper's per-rustc `target/` touch would rather skip its GC
/// bookkeeping row than stall a compile behind another writer.
const BEST_EFFORT_BUSY_TIMEOUT: Duration = Duration::from_millis(50);

thread_local! {
    /// Count of successful state-store opens **on this thread**.
    ///
    /// Kept from the redb era: it makes "this path acquires the database
    /// at most once" (soldr#2224) an assertable property. Per-thread so
    /// concurrent libtest cases don't turn assertions into coin flips.
    static OPEN_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Snapshot of this thread's successful-open count. Take a reading before
/// and after a call to learn how many times it acquired the state database.
pub fn state_db_open_count() -> u64 {
    OPEN_COUNT.with(|count| count.get())
}

/// Owns one SQLite connection to the shared state store. Derefs to
/// [`Connection`] so call sites read as plain rusqlite code.
pub struct StateDbHandle {
    conn: Connection,
}

impl Deref for StateDbHandle {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

fn sqlite_io(error: rusqlite::Error) -> std::io::Error {
    std::io::Error::other(format!("sqlite: {}", describe_sqlite_error(&error)))
}

/// True for the two `rusqlite::Error` shapes SQLite uses for write
/// contention: busy — which covers both a timed-out wait (extended code 5)
/// and the instant WAL snapshot conflict (517) that never waits at all —
/// and locked (a same-connection conflict, e.g. a table locked by an open
/// statement).
///
/// The single busy-detection implementation for this codebase (soldr#3290):
/// `crates/soldr-cli/src/broker_lease.rs` used to carry its own private
/// copy against a different database (`lease.sqlite3`) — a soldr#2741-shaped
/// divergence where two call sites answer "is this contention?" and could
/// silently disagree. Callers needing the retry behavior that copy also had
/// build it on top of this predicate rather than re-matching error codes.
pub fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

/// `error.to_string()` plus, for a `SqliteFailure`, the numeric SQLite
/// extended code and its name when recognized (soldr#3288's missing
/// diagnostic: `517` (`SQLITE_BUSY_SNAPSHOT`, an instant, unwaitable
/// upgrade conflict) reads identically to `5` (plain `SQLITE_BUSY`, a
/// genuine timed-out wait) unless the extended code is logged).
pub fn describe_sqlite_error(error: &rusqlite::Error) -> String {
    let text = error.to_string();
    let rusqlite::Error::SqliteFailure(code, _) = error else {
        return text;
    };
    let name = match code.extended_code {
        517 => " SQLITE_BUSY_SNAPSHOT",
        261 => " SQLITE_BUSY_RECOVERY",
        773 => " SQLITE_BUSY_TIMEOUT",
        5 => " SQLITE_BUSY",
        _ => "",
    };
    format!("{text} (sqlite extended code {}{name})", code.extended_code)
}

/// Open the shared state store with the correctness-critical busy budget.
/// Parent directories are created; the full schema is ensured.
pub fn open_state_db(path: &Path) -> std::io::Result<StateDbHandle> {
    open_with(path, REQUIRED_BUSY_TIMEOUT)
}

/// Open for a latency-critical, losable write (issue #1814): identical to
/// [`open_state_db`] but a contended write waits at most
/// [`BEST_EFFORT_BUSY_TIMEOUT`] before erroring, so the caller can skip
/// its bookkeeping instead of stalling a rustc invocation.
pub fn open_state_db_best_effort(path: &Path) -> std::io::Result<StateDbHandle> {
    open_with(path, BEST_EFFORT_BUSY_TIMEOUT)
}

/// In-memory store with the full schema — for tests and callers that want
/// registry semantics without touching disk.
pub fn open_state_db_in_memory() -> std::io::Result<StateDbHandle> {
    let mut conn = Connection::open_in_memory().map_err(sqlite_io)?;
    conn.set_transaction_behavior(TransactionBehavior::Immediate);
    ensure_schema(&conn).map_err(sqlite_io)?;
    Ok(StateDbHandle { conn })
}

fn open_with(path: &Path, busy: Duration) -> std::io::Result<StateDbHandle> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    remove_legacy_redb_sibling(path);
    let mut conn = Connection::open(path).map_err(sqlite_io)?;
    // BEGIN IMMEDIATE for every transaction on this handle — see the
    // module docs above (soldr#3288/#3290). This is the one place that
    // matters: `StateDbHandle` has no `DerefMut`, so nothing downstream
    // can flip it back to the default deferred behavior.
    conn.set_transaction_behavior(TransactionBehavior::Immediate);
    conn.busy_timeout(busy).map_err(sqlite_io)?;
    // WAL is what buys reader/writer concurrency; NORMAL synchronous is
    // the documented safe pairing with WAL (fsync on checkpoint, not on
    // every commit) and everything in this store is reconstructible
    // bookkeeping. `journal_mode` returns a row, so it must be queried.
    let _mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(sqlite_io)?;
    conn.execute_batch("PRAGMA synchronous=NORMAL;")
        .map_err(sqlite_io)?;
    ensure_schema(&conn).map_err(sqlite_io)?;
    OPEN_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    Ok(StateDbHandle { conn })
}

/// Every table in the shared store, created idempotently at open. One
/// schema site instead of per-module init transactions; a `CREATE TABLE
/// IF NOT EXISTS` on an existing table is a catalog lookup, not a write.
///
/// `u64` keys/counters from the redb era are stored bit-cast as SQLite
/// `INTEGER` (i64). Only equality is ever used on those keys, which the
/// bit-cast preserves.
fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS target_registry_targets (
             key   TEXT PRIMARY KEY,
             value INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS cargo_debug_default_warning_repos (
             key   TEXT PRIMARY KEY,
             value INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS meta (
             key   TEXT PRIMARY KEY,
             value INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS daemon_builds (
             key   INTEGER PRIMARY KEY,
             value BLOB NOT NULL
         );
         CREATE TABLE IF NOT EXISTS daemon_events (
             key   INTEGER PRIMARY KEY,
             value BLOB NOT NULL
         );
         CREATE TABLE IF NOT EXISTS daemon_meta (
             key   TEXT PRIMARY KEY,
             value INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS cook_index_v2 (
             key   BLOB PRIMARY KEY,
             value BLOB NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS unit_memory_history (
             key   TEXT PRIMARY KEY,
             value BLOB NOT NULL
         ) WITHOUT ROWID;",
    )
}

/// Delete a legacy redb store sitting beside the SQLite file. One-time
/// per machine; quiet via tracing (never stdout/stderr — the store is
/// opened from `--json` paths whose output must stay parseable, #2554).
fn remove_legacy_redb_sibling(path: &Path) {
    let legacy = path.with_file_name("state.redb");
    if !legacy.exists() {
        return;
    }
    match std::fs::remove_file(&legacy) {
        Ok(()) => tracing::info!(
            legacy = %legacy.display(),
            "removed legacy redb state store; bookkeeping resets, cook artifacts unaffected"
        ),
        Err(error) => tracing::warn!(
            legacy = %legacy.display(),
            %error,
            "could not remove legacy redb state store"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_schema_and_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("nested").join("state.sqlite3");
        let before = state_db_open_count();
        let handle = open_state_db(&db_path).expect("open");
        assert_eq!(state_db_open_count(), before + 1);
        // Every table exists without any module-level init.
        for table in [
            "target_registry_targets",
            "cargo_debug_default_warning_repos",
            "meta",
            "daemon_builds",
            "daemon_events",
            "daemon_meta",
            "cook_index_v2",
        ] {
            let count: i64 = handle
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect(table);
            assert_eq!(count, 0, "{table} should exist and be empty");
        }
    }

    #[test]
    fn concurrent_reader_and_writer_handles_coexist() {
        // The property redb could not give us: two live handles on one
        // file, reads served while another handle writes.
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("state.sqlite3");
        let writer = open_state_db(&db_path).expect("writer");
        let reader = open_state_db(&db_path).expect("reader");
        writer
            .execute(
                "INSERT INTO target_registry_targets(key, value) VALUES(?1, ?2)",
                rusqlite::params!["/some/target", 42_i64],
            )
            .expect("write");
        let value: i64 = reader
            .query_row(
                "SELECT value FROM target_registry_targets WHERE key = ?1",
                ["/some/target"],
                |row| row.get(0),
            )
            .expect("read while writer handle is live");
        assert_eq!(value, 42);
    }

    #[test]
    fn legacy_redb_sibling_is_removed_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join("state.redb");
        std::fs::write(&legacy, b"old redb bytes").expect("seed legacy");
        let db_path = dir.path().join("state.sqlite3");
        let _handle = open_state_db(&db_path).expect("open");
        assert!(
            !legacy.exists(),
            "legacy redb store must be deleted on first sqlite open"
        );
    }

    #[test]
    fn is_busy_matches_database_busy_and_locked_only() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                extended_code: 517,
            },
            None,
        );
        let locked = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseLocked,
                extended_code: 6,
            },
            None,
        );
        let other = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::PermissionDenied,
                extended_code: 3,
            },
            None,
        );
        assert!(is_busy(&busy));
        assert!(is_busy(&locked));
        assert!(!is_busy(&other));
        assert!(!is_busy(&rusqlite::Error::QueryReturnedNoRows));
    }

    #[test]
    fn describe_sqlite_error_names_busy_snapshot() {
        // soldr#3288: 517 (SQLITE_BUSY | 2<<8) is the instant, unwaitable
        // WAL-upgrade conflict; discarding it made this take a source
        // dive instead of a two-minute log read.
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                extended_code: 517,
            },
            None,
        );
        let text = describe_sqlite_error(&error);
        assert!(text.contains("517"), "{text}");
        assert!(text.contains("SQLITE_BUSY_SNAPSHOT"), "{text}");

        // Plain SQLITE_BUSY (5) must not be misnamed as the snapshot variant.
        let plain_busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                extended_code: 5,
            },
            None,
        );
        let plain_text = describe_sqlite_error(&plain_busy);
        assert!(plain_text.contains("SQLITE_BUSY"), "{plain_text}");
        assert!(!plain_text.contains("SQLITE_BUSY_SNAPSHOT"), "{plain_text}");
    }

    /// RED -> GREEN for soldr#3288/#3290: deterministically reproduces the
    /// WAL-upgrade conflict rather than relying on volume or timing races.
    ///
    /// `a` opens a transaction and reads (the shape every real read-then-write
    /// site on this store has). While `a`'s transaction is still open, `b`
    /// tries an autocommit write on a second handle to the same file.
    ///
    /// * Under the old `BEGIN DEFERRED` default, `a`'s `BEGIN` takes no
    ///   lock, so `b`'s write proceeds and commits immediately — the
    ///   `recv_timeout` below returns `Ok` right away, and `a`'s later
    ///   write, which has to upgrade a now-stale read snapshot, fails with
    ///   `SQLITE_BUSY_SNAPSHOT` (rendered by rusqlite as "database is
    ///   locked", indistinguishable from a real timeout).
    /// * Under `BEGIN IMMEDIATE` (this module's fix), `a`'s `BEGIN` takes
    ///   the write lock up front, so `b`'s write blocks behind it —
    ///   `recv_timeout` below times out — and `a`'s write and commit
    ///   succeed without contention. `b` then unblocks and succeeds too,
    ///   comfortably inside its own 5 s busy timeout.
    #[test]
    fn immediate_transaction_avoids_snapshot_busy_on_upgrade() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("state.sqlite3");

        let a = open_state_db(&db_path).expect("open a");
        let b = open_state_db(&db_path).expect("open b");

        let tx = a.unchecked_transaction().expect("begin a's transaction");
        let _seen: i64 = tx
            .query_row("SELECT COUNT(*) FROM daemon_meta", [], |row| row.get(0))
            .expect("a's read, pinning a snapshot under the deferred-era shape");

        let (done_tx, done_rx) = mpsc::channel();
        let writer_b = std::thread::spawn(move || {
            b.execute(
                "INSERT INTO daemon_meta(key, value) VALUES(?1, ?2)",
                rusqlite::params!["from-b", 2_i64],
            )
            .expect("b's autocommit write must eventually succeed");
            let _ = done_tx.send(());
        });

        // Give `b` ample time to commit if it can. Under BEGIN DEFERRED it can
        // (a holds no lock), so this returns early; under BEGIN IMMEDIATE b
        // is blocked behind a's write lock and this times out.
        let b_finished_early = done_rx.recv_timeout(Duration::from_millis(400)).is_ok();

        // Attempt a's write *before* asserting on `b`, so that a regression
        // fails with the real production symptom — the exact error soldr#3288
        // hit in CI, extended code included — rather than a proxy assertion.
        if let Err(error) = tx.execute(
            "INSERT INTO daemon_meta(key, value) VALUES(?1, ?2)",
            rusqlite::params!["from-a", 1_i64],
        ) {
            panic!(
                "a's read-then-write upgrade failed: {} — this is soldr#3288's \
                 `database is locked` (b committed while a's transaction was open, \
                 the BEGIN DEFERRED shape)",
                describe_sqlite_error(&error)
            );
        }
        tx.commit().expect("a commits");
        assert!(
            !b_finished_early,
            "b's write must block behind a's still-open transaction under BEGIN IMMEDIATE"
        );

        writer_b.join().expect("b's thread must not panic");

        let count: i64 = a
            .query_row("SELECT COUNT(*) FROM daemon_meta", [], |row| row.get(0))
            .expect("count rows");
        assert_eq!(count, 2, "both a's and b's rows must be present");
    }
}
