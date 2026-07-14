//! Redb-backed persistent state for soldr.
//!
//! New state that is not relational can land here without growing ad-hoc
//! marker files under `~/.soldr/`.

use crate::cache_lib::redb_lock::{open_state_db, StateDbHandle};
#[cfg(test)]
use redb::ReadableDatabase;
#[cfg(test)]
use redb::ReadableTableMetadata;
use redb::{ReadableTable, TableDefinition};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

/// Default redb file name under `~/.soldr/`.
pub const STATE_DB_FILE: &str = "state.redb";

/// Keep warning rows for recently used repositories. Active repositories refresh
/// their timestamp on every soldr cargo invocation, so only old/inactive rows
/// are eligible for cleanup.
pub const CARGO_DEBUG_WARNING_TTL_SECONDS: u64 = 180 * 24 * 60 * 60;

/// At most one cleanup pass per day.
pub const CARGO_DEBUG_WARNING_CLEANUP_INTERVAL_SECONDS: u64 = 24 * 60 * 60;

const CARGO_DEBUG_WARNING_REPOS: TableDefinition<&str, u64> =
    TableDefinition::new("cargo_debug_default_warning_repos");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const CARGO_DEBUG_WARNING_LAST_CLEANUP_KEY: &str = "cargo_debug_default_warning_last_cleanup";

#[derive(Debug, Error)]
pub enum StateDbError {
    #[error("redb database error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("system clock before unix epoch: {0}")]
    Clock(String),
}

pub struct StateDb {
    db: StateDbHandle,
}

impl StateDb {
    /// Open or create the redb state database. Parent directories are created
    /// automatically.
    pub fn open(path: &Path) -> Result<Self, StateDbError> {
        let db = open_state_db(path)?;
        Ok(Self { db })
    }

    /// Returns true only for the first time a repository path is observed
    /// before it ages out of the state DB.
    pub fn should_emit_cargo_debug_default_warning(
        &self,
        repo_path: &Path,
    ) -> Result<bool, StateDbError> {
        self.should_emit_cargo_debug_default_warning_with_time(repo_path, current_unix_seconds()?)
    }

    pub fn should_emit_cargo_debug_default_warning_with_time(
        &self,
        repo_path: &Path,
        unix_seconds: u64,
    ) -> Result<bool, StateDbError> {
        let key = path_key(repo_path);
        let write_txn = self.db.begin_write()?;

        let should_emit = {
            let mut table = write_txn.open_table(CARGO_DEBUG_WARNING_REPOS)?;
            let existed = table.get(key.as_str())?.is_some();
            table.insert(key.as_str(), &unix_seconds)?;
            !existed
        };

        cleanup_cargo_debug_warnings_if_due(&write_txn, unix_seconds)?;
        write_txn.commit()?;

        Ok(should_emit)
    }

    #[cfg(test)]
    fn cargo_debug_warning_repo_count(&self) -> Result<u64, StateDbError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CARGO_DEBUG_WARNING_REPOS)?;
        Ok(table.len()?)
    }
}

fn cleanup_cargo_debug_warnings_if_due(
    write_txn: &redb::WriteTransaction,
    unix_seconds: u64,
) -> Result<(), StateDbError> {
    let cleanup_due = {
        let mut meta = write_txn.open_table(META)?;
        let last_cleanup = meta
            .get(CARGO_DEBUG_WARNING_LAST_CLEANUP_KEY)?
            .map(|value| value.value())
            .unwrap_or(0);
        let due = last_cleanup == 0
            || unix_seconds.saturating_sub(last_cleanup)
                >= CARGO_DEBUG_WARNING_CLEANUP_INTERVAL_SECONDS;
        if due {
            meta.insert(CARGO_DEBUG_WARNING_LAST_CLEANUP_KEY, &unix_seconds)?;
        }
        due
    };

    if !cleanup_due {
        return Ok(());
    }

    let cutoff = unix_seconds.saturating_sub(CARGO_DEBUG_WARNING_TTL_SECONDS);
    let mut table = write_txn.open_table(CARGO_DEBUG_WARNING_REPOS)?;
    let stale_keys = {
        let mut keys = Vec::new();
        for row in table.iter()? {
            let (key, last_seen) = row?;
            if last_seen.value() < cutoff {
                keys.push(key.value().to_string());
            }
        }
        keys
    };
    for key in stale_keys {
        table.remove(key.as_str())?;
    }

    Ok(())
}

fn current_unix_seconds() -> Result<u64, StateDbError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| StateDbError::Clock(e.to_string()))?;
    Ok(duration.as_secs())
}

fn path_key(path: &Path) -> String {
    let normalized: PathBuf = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let key = normalized.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    let key = key.to_ascii_lowercase();
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_debug_warning_is_once_per_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let db = StateDb::open(&dir.path().join("state.redb")).unwrap();

        assert!(db
            .should_emit_cargo_debug_default_warning_with_time(&repo, 10)
            .unwrap());
        assert!(!db
            .should_emit_cargo_debug_default_warning_with_time(&repo, 11)
            .unwrap());
        assert_eq!(db.cargo_debug_warning_repo_count().unwrap(), 1);
    }

    #[test]
    fn cargo_debug_warning_cleanup_removes_old_inactive_repos() {
        let dir = tempfile::tempdir().unwrap();
        let stale_repo = dir.path().join("stale");
        let active_repo = dir.path().join("active");
        std::fs::create_dir_all(&stale_repo).unwrap();
        std::fs::create_dir_all(&active_repo).unwrap();
        let db = StateDb::open(&dir.path().join("state.redb")).unwrap();

        assert!(db
            .should_emit_cargo_debug_default_warning_with_time(&stale_repo, 1)
            .unwrap());

        let later =
            CARGO_DEBUG_WARNING_TTL_SECONDS + CARGO_DEBUG_WARNING_CLEANUP_INTERVAL_SECONDS + 10;
        assert!(db
            .should_emit_cargo_debug_default_warning_with_time(&active_repo, later)
            .unwrap());

        assert_eq!(db.cargo_debug_warning_repo_count().unwrap(), 1);
        assert!(!db
            .should_emit_cargo_debug_default_warning_with_time(&active_repo, later + 1)
            .unwrap());
    }
}
