//! SQLite-backed process registry with in-memory HashMap and crash recovery.
//!
//! The [`Registry`] maintains a primary map keyed by `(pid, created_at_ms)` and
//! a secondary index by `pid` alone (latest entry).  Every mutation is
//! write-through to a SQLite WAL database so that tracked processes survive
//! daemon restarts.  On open, stale entries (processes that no longer exist or
//! whose creation time no longer matches) are purged automatically.

use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use sysinfo::{Pid, ProcessRefreshKind, System};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A tracked process entry in the registry.
#[derive(Debug, Clone)]
pub struct TrackedEntry {
    pub pid: u32,
    /// `created_at * 1000`, truncated to `u64`.
    pub created_at_ms: u64,
    /// `"subprocess"` or `"pty"`.
    pub kind: String,
    pub command: String,
    pub cwd: String,
    pub originator: String,
    /// `"contained"` or `"detached"`.
    pub containment: String,
    /// Unix timestamp (fractional seconds).
    pub registered_at: f64,
}

/// Thread-safe process registry with SQLite write-through.
pub struct Registry {
    /// Primary map: `(pid, created_at_ms)` → `TrackedEntry`.
    processes: Mutex<HashMap<(u32, u64), TrackedEntry>>,
    /// Secondary index: `pid` → `created_at_ms` (latest entry for each PID).
    by_pid: Mutex<HashMap<u32, u64>>,
    /// SQLite database connection.
    db: Mutex<Connection>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a fractional-seconds `created_at` to milliseconds.
pub fn created_at_to_ms(created_at: f64) -> u64 {
    (created_at * 1000.0) as u64
}

/// Maps returned by [`load_from_db`]: primary `(pid, created_at_ms)` map
/// and secondary `pid → created_at_ms` index.
type RegistryMaps = (HashMap<(u32, u64), TrackedEntry>, HashMap<u32, u64>);

/// Load all rows from the `tracked_processes` table into memory.
fn load_from_db(conn: &Connection) -> Result<RegistryMaps, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT pid, created_at_ms, kind, command, cwd, originator, containment, registered_at \
         FROM tracked_processes",
    )?;

    let mut processes: HashMap<(u32, u64), TrackedEntry> = HashMap::new();
    let mut by_pid: HashMap<u32, u64> = HashMap::new();

    let rows = stmt.query_map([], |row| {
        Ok(TrackedEntry {
            pid: row.get(0)?,
            created_at_ms: row.get(1)?,
            kind: row.get(2)?,
            command: row.get(3)?,
            cwd: row.get(4)?,
            originator: row.get(5)?,
            containment: row.get(6)?,
            registered_at: row.get(7)?,
        })
    })?;

    for row in rows {
        let entry = row?;
        let key = (entry.pid, entry.created_at_ms);
        by_pid.insert(entry.pid, entry.created_at_ms);
        processes.insert(key, entry);
    }

    Ok((processes, by_pid))
}

// ---------------------------------------------------------------------------
// Registry implementation
// ---------------------------------------------------------------------------

impl Registry {
    /// Open (or create) the registry backed by the SQLite database at
    /// `db_path`.
    ///
    /// On open the table is created if it does not exist, existing rows are
    /// loaded, and **crash recovery** runs: every row is validated against the
    /// OS and stale entries are purged.
    pub fn open(db_path: &Path) -> Result<Self, rusqlite::Error> {
        // Ensure parent directories exist.
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let conn = Connection::open(db_path)?;

        // Enable WAL journal mode for better concurrency.
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // Create the table if it does not exist.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tracked_processes (
                pid            INTEGER NOT NULL,
                created_at_ms  INTEGER NOT NULL,
                kind           TEXT    NOT NULL,
                command        TEXT    NOT NULL,
                cwd            TEXT    NOT NULL DEFAULT '',
                originator     TEXT    NOT NULL DEFAULT '',
                containment    TEXT    NOT NULL DEFAULT 'contained',
                registered_at  REAL    NOT NULL,
                PRIMARY KEY (pid, created_at_ms)
            );",
        )?;

        // Load existing rows.
        let (mut processes, mut by_pid) = load_from_db(&conn)?;

        // --- Crash recovery: validate each entry against the OS. -----------
        let mut stale_keys = Vec::new();
        let mut system = System::new();

        for (&(pid, created_at_ms), _entry) in processes.iter() {
            let sysinfo_pid = Pid::from_u32(pid);
            system.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());

            match system.process(sysinfo_pid) {
                Some(proc) => {
                    // Process exists — verify creation time matches.
                    let proc_start_ms = proc.start_time() * 1000;
                    // Allow 2-second tolerance for creation-time comparison.
                    if proc_start_ms.abs_diff(created_at_ms) > 2000 {
                        stale_keys.push((pid, created_at_ms)); // PID reused
                    }
                }
                None => {
                    stale_keys.push((pid, created_at_ms)); // process gone
                }
            }
        }

        // Purge stale entries from memory and SQLite.
        for key in &stale_keys {
            processes.remove(key);
            by_pid.remove(&key.0);
            conn.execute(
                "DELETE FROM tracked_processes WHERE pid = ?1 AND created_at_ms = ?2",
                params![key.0, key.1],
            )
            .ok();
        }

        tracing::info!(
            "registry recovery: loaded {} processes, purged {} stale",
            processes.len(),
            stale_keys.len()
        );

        Ok(Self {
            processes: Mutex::new(processes),
            by_pid: Mutex::new(by_pid),
            db: Mutex::new(conn),
        })
    }

    /// Register a tracked process entry.
    ///
    /// If the same PID already exists with a different `created_at_ms` the old
    /// entry is replaced (PID reuse).
    pub fn register(&self, entry: TrackedEntry) -> Result<(), rusqlite::Error> {
        let mut by_pid = self.by_pid.lock().unwrap();
        let mut processes = self.processes.lock().unwrap();
        let db = self.db.lock().unwrap();

        // Handle PID reuse: remove old entry if the creation time differs.
        if let Some(&old_created) = by_pid.get(&entry.pid) {
            if old_created != entry.created_at_ms {
                tracing::warn!(
                    pid = entry.pid,
                    old_created_at_ms = old_created,
                    new_created_at_ms = entry.created_at_ms,
                    "PID reuse detected, replacing old entry"
                );
                processes.remove(&(entry.pid, old_created));
                db.execute(
                    "DELETE FROM tracked_processes WHERE pid = ?1 AND created_at_ms = ?2",
                    params![entry.pid, old_created],
                )?;
            }
        }

        // Write to SQLite.
        db.execute(
            "INSERT OR REPLACE INTO tracked_processes \
             (pid, created_at_ms, kind, command, cwd, originator, containment, registered_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                entry.pid,
                entry.created_at_ms,
                entry.kind,
                entry.command,
                entry.cwd,
                entry.originator,
                entry.containment,
                entry.registered_at,
            ],
        )?;

        tracing::debug!(
            pid = entry.pid,
            created_at_ms = entry.created_at_ms,
            command = %entry.command,
            "registered process"
        );

        // Update in-memory maps.
        by_pid.insert(entry.pid, entry.created_at_ms);
        processes.insert((entry.pid, entry.created_at_ms), entry);

        Ok(())
    }

    /// Unregister a tracked process by PID.
    ///
    /// Returns `true` if the entry existed and was removed, `false` otherwise.
    pub fn unregister(&self, pid: u32) -> bool {
        let mut by_pid = self.by_pid.lock().unwrap();
        let mut processes = self.processes.lock().unwrap();
        let db = self.db.lock().unwrap();

        let Some(created_at_ms) = by_pid.remove(&pid) else {
            return false;
        };

        processes.remove(&(pid, created_at_ms));

        db.execute(
            "DELETE FROM tracked_processes WHERE pid = ?1 AND created_at_ms = ?2",
            params![pid, created_at_ms],
        )
        .ok();

        true
    }

    /// Unregister a tracked process only if the creation time still matches.
    ///
    /// This is used by background child waiters so a fast PID reuse does not
    /// accidentally remove a newer process registration.
    pub fn unregister_exact(&self, pid: u32, created_at_ms: u64) -> bool {
        let mut by_pid = self.by_pid.lock().unwrap();
        let mut processes = self.processes.lock().unwrap();
        let db = self.db.lock().unwrap();

        let removed = processes.remove(&(pid, created_at_ms)).is_some();
        if !removed {
            return false;
        }

        if by_pid.get(&pid) == Some(&created_at_ms) {
            by_pid.remove(&pid);
        }

        db.execute(
            "DELETE FROM tracked_processes WHERE pid = ?1 AND created_at_ms = ?2",
            params![pid, created_at_ms],
        )
        .ok();

        true
    }

    /// Return a clone of all tracked entries, sorted by `registered_at`.
    pub fn list_all(&self) -> Vec<TrackedEntry> {
        let processes = self.processes.lock().unwrap();
        let mut entries: Vec<TrackedEntry> = processes.values().cloned().collect();
        entries.sort_by(|a, b| {
            a.registered_at
                .partial_cmp(&b.registered_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entries
    }

    /// Return entries whose `originator` starts with `"{tool}:"`.
    pub fn list_by_originator(&self, tool: &str) -> Vec<TrackedEntry> {
        let prefix = format!("{tool}:");
        self.list_all()
            .into_iter()
            .filter(|e| e.originator.starts_with(&prefix))
            .collect()
    }

    /// Return the number of tracked processes.
    pub fn count(&self) -> usize {
        self.processes.lock().unwrap().len()
    }

    /// Validate all tracked entries against the OS and remove stale ones.
    ///
    /// A process is considered stale if it no longer exists or if its OS-level
    /// creation time no longer matches the recorded `created_at_ms` (within a
    /// 2-second tolerance).
    pub fn validate_against_os(&self) {
        let mut system = System::new();
        let mut stale_keys = Vec::new();

        {
            let processes = self.processes.lock().unwrap();
            for (&(pid, created_at_ms), _entry) in processes.iter() {
                let sysinfo_pid = Pid::from_u32(pid);
                system.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());

                match system.process(sysinfo_pid) {
                    Some(proc) => {
                        let proc_start_ms = proc.start_time() * 1000;
                        if proc_start_ms.abs_diff(created_at_ms) > 2000 {
                            stale_keys.push((pid, created_at_ms));
                        }
                    }
                    None => {
                        stale_keys.push((pid, created_at_ms));
                    }
                }
            }
        }

        if stale_keys.is_empty() {
            return;
        }

        let mut by_pid = self.by_pid.lock().unwrap();
        let mut processes = self.processes.lock().unwrap();
        let db = self.db.lock().unwrap();

        for &(pid, created_at_ms) in &stale_keys {
            processes.remove(&(pid, created_at_ms));
            // Only remove from by_pid if the entry still maps to this created_at_ms.
            if by_pid.get(&pid) == Some(&created_at_ms) {
                by_pid.remove(&pid);
            }
            db.execute(
                "DELETE FROM tracked_processes WHERE pid = ?1 AND created_at_ms = ?2",
                params![pid, created_at_ms],
            )
            .ok();
            tracing::info!(pid, created_at_ms, "purged stale process from registry");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_entry(pid: u32, created_at_ms: u64, command: &str) -> TrackedEntry {
        TrackedEntry {
            pid,
            created_at_ms,
            kind: "subprocess".to_string(),
            command: command.to_string(),
            cwd: "/tmp".to_string(),
            originator: "test:unit".to_string(),
            containment: "contained".to_string(),
            registered_at: created_at_ms as f64 / 1000.0,
        }
    }

    #[test]
    fn test_register_and_list() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let reg = Registry::open(&db).unwrap();

        reg.register(make_entry(1, 1000, "cmd1")).unwrap();
        reg.register(make_entry(2, 2000, "cmd2")).unwrap();
        reg.register(make_entry(3, 3000, "cmd3")).unwrap();

        let all = reg.list_all();
        assert_eq!(all.len(), 3);
        assert_eq!(reg.count(), 3);
    }

    #[test]
    fn test_unregister_removes() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let reg = Registry::open(&db).unwrap();

        reg.register(make_entry(42, 5000, "ls -la")).unwrap();
        assert_eq!(reg.count(), 1);

        let removed = reg.unregister(42);
        assert!(removed);
        assert_eq!(reg.count(), 0);
        assert_eq!(reg.list_all().len(), 0);

        // Unregister again returns false.
        let removed_again = reg.unregister(42);
        assert!(!removed_again);
    }

    #[test]
    fn test_unregister_exact_preserves_newer_pid_reuse() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let reg = Registry::open(&db).unwrap();

        reg.register(make_entry(77, 1000, "old")).unwrap();
        reg.register(make_entry(77, 2000, "new")).unwrap();

        assert!(!reg.unregister_exact(77, 1000));
        let all = reg.list_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].created_at_ms, 2000);

        assert!(reg.unregister_exact(77, 2000));
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn test_pid_reuse_replaces_old() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let reg = Registry::open(&db).unwrap();

        reg.register(make_entry(100, 1000, "old-cmd")).unwrap();
        assert_eq!(reg.count(), 1);

        // Same PID, different created_at_ms → PID reuse.
        reg.register(make_entry(100, 2000, "new-cmd")).unwrap();
        assert_eq!(reg.count(), 1);

        let all = reg.list_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].created_at_ms, 2000);
        assert_eq!(all[0].command, "new-cmd");
    }

    #[test]
    fn test_list_by_originator_filters() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");
        let reg = Registry::open(&db).unwrap();

        let mut e1 = make_entry(1, 1000, "cmd1");
        e1.originator = "codeup:abc".to_string();
        let mut e2 = make_entry(2, 2000, "cmd2");
        e2.originator = "codeup:def".to_string();
        let mut e3 = make_entry(3, 3000, "cmd3");
        e3.originator = "other:xyz".to_string();

        reg.register(e1).unwrap();
        reg.register(e2).unwrap();
        reg.register(e3).unwrap();

        let codeup = reg.list_by_originator("codeup");
        assert_eq!(codeup.len(), 2);

        let other = reg.list_by_originator("other");
        assert_eq!(other.len(), 1);

        let none = reg.list_by_originator("nonexistent");
        assert_eq!(none.len(), 0);
    }

    #[test]
    fn test_persistence_across_reopen() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("test.db");

        // Register entries in first instance.
        {
            let reg = Registry::open(&db).unwrap();
            // Use the current process PID and a matching creation time so that
            // crash recovery does not purge the entry on reopen.
            let pid = std::process::id();
            let mut sys = System::new();
            let sysinfo_pid = Pid::from_u32(pid);
            sys.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());
            let created_at_ms = sys
                .process(sysinfo_pid)
                .map(|p| p.start_time() * 1000)
                .unwrap_or(0);

            reg.register(TrackedEntry {
                pid,
                created_at_ms,
                kind: "subprocess".to_string(),
                command: "persist-test".to_string(),
                cwd: "/tmp".to_string(),
                originator: "test:persist".to_string(),
                containment: "contained".to_string(),
                registered_at: 1000.0,
            })
            .unwrap();
            assert_eq!(reg.count(), 1);
        }

        // Reopen — entry should survive because we used the real process PID.
        {
            let reg = Registry::open(&db).unwrap();
            let all = reg.list_all();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].command, "persist-test");
        }
    }

    #[test]
    fn test_created_at_to_ms() {
        assert_eq!(created_at_to_ms(1234.567), 1234567);
        assert_eq!(created_at_to_ms(0.0), 0);
        assert_eq!(created_at_to_ms(1.0), 1000);
    }
}
