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
//! ## Legacy `state.redb`
//!
//! A sibling `state.redb` written by pre-SQLite soldr is deleted on first
//! open. All of its contents are disposable local bookkeeping (target
//! recency rows, build history snapshots, cook index rows — the on-disk
//! cook artifacts are unaffected and re-index on the next `soldr cook`),
//! matching the precedent of the #580/#603 row-format migrations.

use rusqlite::Connection;
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
    std::io::Error::other(format!("sqlite: {error}"))
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
    let conn = Connection::open_in_memory().map_err(sqlite_io)?;
    ensure_schema(&conn).map_err(sqlite_io)?;
    Ok(StateDbHandle { conn })
}

fn open_with(path: &Path, busy: Duration) -> std::io::Result<StateDbHandle> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    remove_legacy_redb_sibling(path);
    let conn = Connection::open(path).map_err(sqlite_io)?;
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
}
