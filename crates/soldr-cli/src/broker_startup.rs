//! Cross-process broker bring-up election.
//!
//! The broker endpoint itself is never stored here: every process derives it
//! from the canonical installed broker path. SQLite coordinates only which
//! process is currently allowed to create that already-known endpoint.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

const SLOT: i64 = 1;
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// Result of one atomic bring-up election attempt.
pub(crate) enum StartupClaim {
    /// This process owns the timed attempt until the guard drops.
    Owner(StartupLease),
    /// Another process owns an unexpired attempt. Retry after this bound.
    Contended(Duration),
}

/// Exact-generation lease. Drop never clears a newer process's attempt.
pub(crate) struct StartupLease {
    connection: Connection,
    pid_key: String,
}

/// Try to own broker creation for the executable at `broker_path`.
///
/// The database is a sidecar of the installed executable, so its filesystem
/// permissions naturally follow the installation scope. A machine-managed
/// install can pre-create/own the sidecar; an unprivileged client can still
/// take the pipe fast path but cannot invent a second private coordination
/// location when machine-wide startup is unavailable.
pub(crate) fn try_claim(
    broker_path: &Path,
    program: &str,
    lease_duration: Duration,
    wait_budget: Duration,
) -> Result<StartupClaim, String> {
    let database_path = startup_database_path(broker_path, program)?;
    try_claim_database(&database_path, lease_duration, wait_budget)
}

fn startup_database_path(broker_path: &Path, program: &str) -> Result<PathBuf, String> {
    use sha2::{Digest, Sha256};

    let file_name = broker_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "installed broker path has no usable file name: {}",
                broker_path.display()
            )
        })?;
    let scope = running_process::broker::lifecycle::names_v2::broker_path_scope_hash(broker_path)
        .map_err(|err| format!("derive broker startup scope: {err}"))?;
    let program_hash = hex::encode(Sha256::digest(program.as_bytes()));
    Ok(broker_path.with_file_name(format!(
        ".{file_name}.{scope}-{}.broker-startup.sqlite3",
        &program_hash[..16]
    )))
}

fn try_claim_database(
    database_path: &Path,
    lease_duration: Duration,
    wait_budget: Duration,
) -> Result<StartupClaim, String> {
    let mut connection = Connection::open(database_path).map_err(|err| {
        format!(
            "open broker startup coordinator {}: {err}",
            database_path.display()
        )
    })?;
    connection
        .busy_timeout(SQLITE_BUSY_TIMEOUT.min(wait_budget))
        .map_err(|err| format!("configure broker startup coordinator: {err}"))?;

    let now_ms = unix_time_ms()?;

    // WAL fast path: once one starter has published its lease, every other
    // contender observes it through reads only: no PRAGMA or CREATE is issued
    // before this check. Schema/WAL setup belongs exclusively to the missing
    // or stale writer path below.
    let initialized = match current_deadline_if_initialized(&connection)? {
        DatabaseState::Initialized(current_deadline) => {
            if current_deadline.is_some_and(|deadline| deadline > now_ms) {
                return Ok(StartupClaim::Contended(Duration::from_millis(
                    current_deadline.unwrap().saturating_sub(now_ms),
                )));
            }
            true
        }
        DatabaseState::Uninitialized => false,
    };

    if !initialized {
        if let Err(err) = connection.execute_batch("PRAGMA journal_mode = WAL;") {
            return busy_or_error(
                err,
                wait_budget,
                "enable WAL for broker startup coordinator",
            );
        }
    }
    connection
        .execute_batch("PRAGMA synchronous = NORMAL;")
        .map_err(|err| format!("configure broker startup durability: {err}"))?;

    // Only an absent/stale observation attempts the single-writer election.
    // Re-read under BEGIN IMMEDIATE because another reader may have won in the
    // interval between the optimistic read above and this transaction.
    let transaction = match connection.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(transaction) => transaction,
        Err(err) => return busy_or_error(err, wait_budget, "lock broker startup coordinator"),
    };
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS broker_startup (\
                 slot INTEGER PRIMARY KEY CHECK (slot = 1),\
                 pid_key TEXT NOT NULL\
             );",
        )
        .map_err(|err| format!("initialize broker startup coordinator: {err}"))?;
    let locked_now_ms = unix_time_ms()?;
    if let Some(current_deadline) = current_deadline(&transaction)? {
        if current_deadline > locked_now_ms {
            transaction
                .commit()
                .map_err(|err| format!("release broker startup election: {err}"))?;
            return Ok(StartupClaim::Contended(Duration::from_millis(
                current_deadline.saturating_sub(locked_now_ms),
            )));
        }
    }

    let lease_ms = u64::try_from(lease_duration.as_millis()).unwrap_or(u64::MAX);
    let deadline_ms = locked_now_ms.saturating_add(lease_ms.max(1));
    let pid_key = new_pid_key(locked_now_ms, deadline_ms);
    transaction
        .execute(
            "INSERT INTO broker_startup(slot, pid_key) VALUES (?1, ?2) \
             ON CONFLICT(slot) DO UPDATE SET pid_key = excluded.pid_key",
            params![SLOT, &pid_key],
        )
        .map_err(|err| format!("claim broker startup ownership: {err}"))?;
    transaction
        .commit()
        .map_err(|err| format!("commit broker startup ownership: {err}"))?;
    Ok(StartupClaim::Owner(StartupLease {
        connection,
        pid_key,
    }))
}

enum DatabaseState {
    Uninitialized,
    Initialized(Option<u64>),
}

fn current_deadline_if_initialized(connection: &Connection) -> Result<DatabaseState, String> {
    let initialized = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema \
             WHERE type = 'table' AND name = 'broker_startup')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|err| format!("inspect broker startup coordinator: {err}"))?;
    if !initialized {
        return Ok(DatabaseState::Uninitialized);
    }
    current_deadline(connection).map(DatabaseState::Initialized)
}

fn busy_or_error(
    err: rusqlite::Error,
    wait_budget: Duration,
    context: &str,
) -> Result<StartupClaim, String> {
    if matches!(
        err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    ) {
        return Ok(StartupClaim::Contended(
            POLITE_RETRY.min(wait_budget).max(Duration::from_millis(1)),
        ));
    }
    Err(format!("{context}: {err}"))
}

const POLITE_RETRY: Duration = Duration::from_millis(50);

fn current_deadline(connection: &Connection) -> Result<Option<u64>, String> {
    let current: Option<String> = connection
        .query_row(
            "SELECT pid_key FROM broker_startup WHERE slot = ?1",
            params![SLOT],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| format!("read broker startup owner: {err}"))?;
    Ok(current.as_deref().and_then(pid_key_deadline))
}

impl Drop for StartupLease {
    fn drop(&mut self) {
        let _ = self.connection.execute(
            "DELETE FROM broker_startup WHERE slot = ?1 AND pid_key = ?2",
            params![SLOT, &self.pid_key],
        );
    }
}

fn new_pid_key(now_ms: u64, deadline_ms: u64) -> String {
    let generation = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default()
        ^ u128::from(std::process::id()).rotate_left(37);
    format!(
        "{}:{generation:032x}:{now_ms}:{deadline_ms}",
        std::process::id()
    )
}

fn pid_key_deadline(pid_key: &str) -> Option<u64> {
    let mut fields = pid_key.split(':');
    let pid = fields.next()?.parse::<u32>().ok()?;
    let generation = fields.next()?;
    let started_ms = fields.next()?.parse::<u64>().ok()?;
    let deadline_ms = fields.next()?.parse::<u64>().ok()?;
    if pid == 0
        || generation.len() != 32
        || !generation
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || deadline_ms <= started_ms
        || fields.next().is_some()
    {
        return None;
    }
    Some(deadline_ms)
}

fn unix_time_ms() -> Result<u64, String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("system clock is before Unix epoch: {err}"))?;
    Ok(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_owner_blocks_then_releases_the_next_process() {
        let temp = tempfile::tempdir().expect("tempdir");
        let database = temp.path().join("startup.sqlite3");
        let first = match try_claim_database(&database, Duration::from_secs(3), SQLITE_BUSY_TIMEOUT)
            .expect("claim")
        {
            StartupClaim::Owner(lease) => lease,
            StartupClaim::Contended(_) => panic!("first contender must own"),
        };
        assert!(matches!(
            try_claim_database(&database, Duration::from_secs(3), SQLITE_BUSY_TIMEOUT)
                .expect("contended claim"),
            StartupClaim::Contended(_)
        ));
        drop(first);
        assert!(matches!(
            try_claim_database(&database, Duration::from_secs(3), SQLITE_BUSY_TIMEOUT)
                .expect("reclaim"),
            StartupClaim::Owner(_)
        ));
    }

    #[test]
    fn one_wal_writer_is_observed_by_many_concurrent_readers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let database = temp.path().join("startup.sqlite3");
        let owner = match try_claim_database(&database, Duration::from_secs(3), SQLITE_BUSY_TIMEOUT)
            .expect("claim")
        {
            StartupClaim::Owner(lease) => lease,
            StartupClaim::Contended(_) => panic!("first contender must own"),
        };

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
        let mut readers = Vec::new();
        for _ in 0..8 {
            let database = database.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            readers.push(std::thread::spawn(move || {
                barrier.wait();
                matches!(
                    try_claim_database(&database, Duration::from_secs(3), SQLITE_BUSY_TIMEOUT)
                        .expect("concurrent WAL read"),
                    StartupClaim::Contended(_)
                )
            }));
        }
        barrier.wait();
        for reader in readers {
            assert!(reader.join().expect("reader thread"));
        }
        drop(owner);
    }

    #[test]
    fn concurrent_first_open_elects_exactly_one_writer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let database = temp.path().join("startup.sqlite3");
        let start = std::sync::Arc::new(std::sync::Barrier::new(9));
        let hold_claims = std::sync::Arc::new(std::sync::Barrier::new(9));
        let (observed, results) = std::sync::mpsc::channel();
        let mut contenders = Vec::new();
        for _ in 0..8 {
            let database = database.clone();
            let start = std::sync::Arc::clone(&start);
            let hold_claims = std::sync::Arc::clone(&hold_claims);
            let observed = observed.clone();
            contenders.push(std::thread::spawn(move || {
                start.wait();
                let claim =
                    try_claim_database(&database, Duration::from_secs(3), SQLITE_BUSY_TIMEOUT)
                        .expect("concurrent first-open claim");
                observed
                    .send(matches!(claim, StartupClaim::Owner(_)))
                    .expect("report claim");
                hold_claims.wait();
                drop(claim);
            }));
        }
        drop(observed);
        start.wait();
        let owners = results.iter().take(8).filter(|owner| *owner).count();
        assert_eq!(owners, 1, "exactly one first opener may become the writer");
        hold_claims.wait();
        for contender in contenders {
            contender.join().expect("contender thread");
        }
    }

    #[test]
    fn stale_or_malformed_owner_is_replaced() {
        let temp = tempfile::tempdir().expect("tempdir");
        let database = temp.path().join("startup.sqlite3");
        let connection = Connection::open(&database).expect("open");
        connection
            .execute_batch(
                "CREATE TABLE broker_startup (\
                     slot INTEGER PRIMARY KEY CHECK (slot = 1),\
                     pid_key TEXT NOT NULL\
                 );\
                 INSERT INTO broker_startup(slot, pid_key) VALUES (1, 'dead-owner');",
            )
            .expect("seed malformed stale owner");
        drop(connection);

        assert!(matches!(
            try_claim_database(&database, Duration::from_secs(3), SQLITE_BUSY_TIMEOUT)
                .expect("replace"),
            StartupClaim::Owner(_)
        ));
    }

    #[test]
    fn pid_key_parser_requires_the_complete_attempt_identity() {
        assert_eq!(
            pid_key_deadline("123:00000000000000000000000000000001:10:11"),
            Some(11)
        );
        assert_eq!(pid_key_deadline("dead-owner:9999999999999"), None);
        assert_eq!(pid_key_deadline("123:abc:10:9999999999999"), None);
        assert_eq!(
            pid_key_deadline("123:00000000000000000000000000000001:10:11:extra"),
            None
        );
    }

    #[test]
    fn sidecar_path_tracks_the_exact_pipe_scope() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("soldr");
        std::fs::write(&path, b"broker").expect("broker fixture");
        let first = startup_database_path(&path, "soldr-daemon").expect("path");
        assert_eq!(first.parent(), path.parent());
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".soldr."));
        assert_eq!(
            first,
            startup_database_path(&path, "soldr-daemon").expect("stable path")
        );
        assert_ne!(
            first,
            startup_database_path(&path, "test-program").expect("test namespace")
        );
    }
}
