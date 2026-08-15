//! SQLite-backed persistent state for soldr.
//!
//! New state that is not relational can land here without growing ad-hoc
//! marker files under `~/.soldr/`.

use crate::cache_lib::state_store::{open_state_db, StateDbHandle};
use rusqlite::{params, OptionalExtension};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

/// Default state-store file name under `~/.soldr/`.
pub const STATE_DB_FILE: &str = "state.sqlite3";

/// Keep warning rows for recently used repositories. Active repositories refresh
/// their timestamp on every soldr cargo invocation, so only old/inactive rows
/// are eligible for cleanup.
pub const CARGO_DEBUG_WARNING_TTL_SECONDS: u64 = 180 * 24 * 60 * 60;

/// At most one cleanup pass per day.
pub const CARGO_DEBUG_WARNING_CLEANUP_INTERVAL_SECONDS: u64 = 24 * 60 * 60;

const CARGO_DEBUG_WARNING_LAST_CLEANUP_KEY: &str = "cargo_debug_default_warning_last_cleanup";

#[derive(Debug, Error)]
pub enum StateDbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("system clock before unix epoch: {0}")]
    Clock(String),
}

pub struct StateDb {
    db: StateDbHandle,
}

impl StateDb {
    /// Open or create the state database. Parent directories are created
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
        let tx = self.db.unchecked_transaction()?;

        let existed: bool = tx
            .query_row(
                "SELECT 1 FROM cargo_debug_default_warning_repos WHERE key = ?1",
                params![key],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        tx.execute(
            "INSERT INTO cargo_debug_default_warning_repos(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, unix_seconds as i64],
        )?;

        cleanup_cargo_debug_warnings_if_due(&tx, unix_seconds)?;
        tx.commit()?;

        Ok(!existed)
    }

    #[cfg(test)]
    fn cargo_debug_warning_repo_count(&self) -> Result<u64, StateDbError> {
        let count: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM cargo_debug_default_warning_repos",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }
}

fn cleanup_cargo_debug_warnings_if_due(
    tx: &rusqlite::Transaction<'_>,
    unix_seconds: u64,
) -> Result<(), StateDbError> {
    let last_cleanup: i64 = tx
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![CARGO_DEBUG_WARNING_LAST_CLEANUP_KEY],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let due = last_cleanup == 0
        || unix_seconds.saturating_sub(last_cleanup as u64)
            >= CARGO_DEBUG_WARNING_CLEANUP_INTERVAL_SECONDS;
    if !due {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![CARGO_DEBUG_WARNING_LAST_CLEANUP_KEY, unix_seconds as i64],
    )?;

    let cutoff = unix_seconds.saturating_sub(CARGO_DEBUG_WARNING_TTL_SECONDS);
    tx.execute(
        "DELETE FROM cargo_debug_default_warning_repos WHERE value < ?1",
        params![cutoff as i64],
    )?;

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
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_debug_warning_is_once_per_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let db = StateDb::open(&dir.path().join(STATE_DB_FILE)).unwrap();

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
        let db = StateDb::open(&dir.path().join(STATE_DB_FILE)).unwrap();

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
