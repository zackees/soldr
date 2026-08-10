//! Service supervisor registry (runpm Phase 2).
//!
//! A "service" is a long-running, daemon-owned, auto-restarting process —
//! the PM2-style unit that `runpm start` creates. Unlike the detachable
//! [`crate::daemon::pipe_sessions`] sessions (which are one-shot and never
//! restarted), a service has a persisted *definition* (command, cwd, env,
//! restart policy) plus *runtime state* (status, pid, restart count, exit
//! info). Definitions and state are written through to a SQLite `services`
//! table so they survive daemon restarts; the in-memory map of
//! `OwnedService` holds the live child process and its log-writer threads.
//!
//! Supervision (restart-on-exit with exponential backoff, a max-restart
//! window, and a min-uptime threshold) runs in the daemon-owned background
//! task [`supervisor_loop`].

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use tracing::{debug, info, warn};

use crate::daemon::services_snapshot::SNAPSHOT_FILE_NAME;

use crate::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode, StreamKind,
};

// ---------------------------------------------------------------------------
// Status enum
// ---------------------------------------------------------------------------

/// Lifecycle status of a service, mirroring the protobuf `ServiceState.status`
/// string field (`"online" | "stopped" | "errored" | "starting"`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ServiceStatus {
    /// Child is running and being supervised.
    Online,
    /// Stopped by an operator; the supervisor will not restart it.
    Stopped,
    /// Crashed too many times too fast; the supervisor gave up.
    Errored,
}

impl ServiceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceStatus::Online => "online",
            ServiceStatus::Stopped => "stopped",
            ServiceStatus::Errored => "errored",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "online" => ServiceStatus::Online,
            "errored" => ServiceStatus::Errored,
            _ => ServiceStatus::Stopped,
        }
    }
}

// ---------------------------------------------------------------------------
// Restart policy
// ---------------------------------------------------------------------------

/// Backoff and max-restart policy applied by the supervisor when a service
/// exits unexpectedly.
#[derive(Clone, Debug)]
pub struct RestartPolicy {
    /// Restart automatically when the child exits unexpectedly.
    pub autorestart: bool,
    /// Stop restarting (and mark `errored`) after this many crashes inside
    /// the max-restart window. `0` means unlimited.
    pub max_restarts: u32,
    /// Base delay before the first restart. Doubles each consecutive crash
    /// (capped) until the service stays up for `min_uptime`.
    pub base_delay: Duration,
    /// A service that stays up at least this long resets the backoff and the
    /// rapid-crash counter.
    pub min_uptime: Duration,
}

impl RestartPolicy {
    pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

    fn from_config(cfg: &ServiceDef) -> Self {
        let base_delay = if cfg.restart_delay_ms == 0 {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(cfg.restart_delay_ms as u64)
        };
        let min_uptime = if cfg.min_uptime_ms == 0 {
            Duration::from_secs(2)
        } else {
            Duration::from_millis(cfg.min_uptime_ms as u64)
        };
        Self {
            autorestart: cfg.autorestart,
            max_restarts: cfg.max_restarts,
            base_delay,
            min_uptime,
        }
    }

    /// Compute the backoff delay for the Nth consecutive rapid crash
    /// (0-based), capped at [`Self::MAX_BACKOFF`].
    pub fn backoff_for(&self, consecutive_crashes: u32) -> Duration {
        let shift = consecutive_crashes.min(16);
        let scaled = self
            .base_delay
            .checked_mul(1u32 << shift.min(16))
            .unwrap_or(Self::MAX_BACKOFF);
        scaled.min(Self::MAX_BACKOFF)
    }
}

// ---------------------------------------------------------------------------
// Persisted definition + state
// ---------------------------------------------------------------------------

/// A persisted service definition: the immutable launch recipe.
#[derive(Clone, Debug)]
pub struct ServiceDef {
    pub name: String,
    pub cmd: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub autorestart: bool,
    pub max_restarts: u32,
    pub restart_delay_ms: u32,
    pub kill_timeout_ms: u32,
    pub min_uptime_ms: u32,
}

impl ServiceDef {
    fn kill_grace(&self) -> Duration {
        if self.kill_timeout_ms == 0 {
            Duration::from_secs(3)
        } else {
            Duration::from_millis(self.kill_timeout_ms as u64)
        }
    }
}

/// A full service record: definition plus mutable runtime state. This is the
/// shape returned to the CLI (mapped to the protobuf `ServiceState`).
#[derive(Clone, Debug)]
pub struct ServiceRecord {
    pub id: u32,
    pub def: ServiceDef,
    pub status: ServiceStatus,
    pub pid: u32,
    pub restart_count: u32,
    pub last_started_at: f64,
    pub last_exited_at: f64,
    pub last_exit_code: i32,
}

// ---------------------------------------------------------------------------
// Live runtime handle
// ---------------------------------------------------------------------------

/// Live state for a service whose child process is currently running.
struct OwnedService {
    process: Arc<NativeProcess>,
    /// When the current incarnation was spawned — used by the supervisor to
    /// apply the min-uptime backoff reset.
    started_at: Instant,
    /// Set when an operator stop/delete/restart is in progress so the
    /// supervisor does not treat the resulting exit as an unexpected crash.
    intentional_stop: Arc<AtomicBool>,
    log_shutdown: Arc<AtomicBool>,
    /// Join handles for the log-writer threads. Held so the threads stay
    /// owned for the lifetime of the live service (they observe
    /// `log_shutdown` / stream EOF to exit); never otherwise read.
    #[allow(dead_code)]
    reader_threads: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl OwnedService {
    fn signal_log_shutdown(&self) {
        self.log_shutdown.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ServiceError {
    /// A service with this name already exists.
    AlreadyExists(String),
    /// No service matched the target.
    NotFound(String),
    /// argv was empty / invalid.
    InvalidConfig(String),
    /// Failed to spawn the child.
    Spawn(String),
    /// SQLite write-through failed.
    Db(String),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::AlreadyExists(n) => write!(f, "service '{n}' already exists"),
            ServiceError::NotFound(t) => write!(f, "no service matched '{t}'"),
            ServiceError::InvalidConfig(m) => write!(f, "invalid service config: {m}"),
            ServiceError::Spawn(m) => write!(f, "failed to spawn service: {m}"),
            ServiceError::Db(m) => write!(f, "service db error: {m}"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<rusqlite::Error> for ServiceError {
    fn from(e: rusqlite::Error) -> Self {
        ServiceError::Db(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// SQLite-backed registry of supervised services with an in-memory map of
/// the live child processes.
pub struct ServiceRegistry {
    db: Mutex<Connection>,
    /// Name → live runtime handle, present only while the child is running.
    live: Mutex<HashMap<String, Arc<OwnedService>>>,
    /// Directory under which per-service log files are written.
    log_dir: PathBuf,
    /// Where save/resurrect persist a JSON snapshot of every row in the
    /// `services` table. Co-located with the SQLite file so the snapshot
    /// shares the same per-scope local directory.
    pub(crate) snapshot_path: PathBuf,
    pub(crate) next_id: AtomicU32,
}

impl ServiceRegistry {
    /// Open (or create) the service registry. `db_path` is the SQLite file
    /// (shared with the process registry); `log_dir` is where per-service
    /// `<name>-out.log` / `<name>-err.log` files are written.
    pub fn open(db_path: &Path, log_dir: PathBuf) -> Result<Self, ServiceError> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::create_dir_all(&log_dir);

        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS services (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                name            TEXT    NOT NULL UNIQUE,
                cmd             TEXT    NOT NULL,
                cwd             TEXT    NOT NULL DEFAULT '',
                env             TEXT    NOT NULL DEFAULT '[]',
                autorestart     INTEGER NOT NULL DEFAULT 1,
                max_restarts    INTEGER NOT NULL DEFAULT 0,
                restart_delay_ms INTEGER NOT NULL DEFAULT 0,
                kill_timeout_ms  INTEGER NOT NULL DEFAULT 0,
                min_uptime_ms    INTEGER NOT NULL DEFAULT 0,
                status          TEXT    NOT NULL DEFAULT 'stopped',
                pid             INTEGER NOT NULL DEFAULT 0,
                restart_count   INTEGER NOT NULL DEFAULT 0,
                last_started_at REAL    NOT NULL DEFAULT 0,
                last_exited_at  REAL    NOT NULL DEFAULT 0,
                last_exit_code  INTEGER NOT NULL DEFAULT 0
            );",
        )?;

        // After a daemon restart no children survive, so any row that claims
        // to be `online` is stale: mark it stopped and clear the pid.
        conn.execute(
            "UPDATE services SET status = 'stopped', pid = 0 WHERE status = 'online'",
            [],
        )?;

        let max_id: u32 = conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM services", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);

        // Snapshot file co-located with the SQLite db (same per-scope
        // local directory).
        let snapshot_path = db_path
            .parent()
            .map(|p| p.join(SNAPSHOT_FILE_NAME))
            .unwrap_or_else(|| PathBuf::from(SNAPSHOT_FILE_NAME));

        Ok(Self {
            db: Mutex::new(conn),
            live: Mutex::new(HashMap::new()),
            log_dir,
            snapshot_path,
            next_id: AtomicU32::new(max_id + 1),
        })
    }

    /// Path to the JSON snapshot file used by `save`/`resurrect`. Always
    /// co-located with the SQLite registry file.
    pub fn snapshot_path(&self) -> &Path {
        &self.snapshot_path
    }

    // -- Persistence helpers -------------------------------------------------

    fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ServiceRecord> {
        let cmd_json: String = row.get("cmd")?;
        let env_json: String = row.get("env")?;
        let cmd: Vec<String> = serde_json::from_str(&cmd_json).unwrap_or_default();
        let env: Vec<(String, String)> = serde_json::from_str(&env_json).unwrap_or_default();
        let status: String = row.get("status")?;
        Ok(ServiceRecord {
            id: row.get("id")?,
            def: ServiceDef {
                name: row.get("name")?,
                cmd,
                cwd: row.get("cwd")?,
                env,
                autorestart: row.get::<_, i64>("autorestart")? != 0,
                max_restarts: row.get("max_restarts")?,
                restart_delay_ms: row.get("restart_delay_ms")?,
                kill_timeout_ms: row.get("kill_timeout_ms")?,
                min_uptime_ms: row.get("min_uptime_ms")?,
            },
            status: ServiceStatus::from_str(&status),
            pid: row.get("pid")?,
            restart_count: row.get("restart_count")?,
            last_started_at: row.get("last_started_at")?,
            last_exited_at: row.get("last_exited_at")?,
            last_exit_code: row.get("last_exit_code")?,
        })
    }

    fn fetch(&self, name: &str) -> Result<Option<ServiceRecord>, ServiceError> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare("SELECT * FROM services WHERE name = ?1")?;
        let mut rows = stmt.query(params![name])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::row_to_record(row)?)),
            None => Ok(None),
        }
    }

    /// Resolve a CLI target (name or numeric id) to a service name.
    fn resolve_target(&self, target: &str) -> Result<Option<String>, ServiceError> {
        let db = self.db.lock().unwrap();
        // Numeric id?
        if let Ok(id) = target.parse::<u32>() {
            let name: Option<String> = db
                .query_row(
                    "SELECT name FROM services WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .ok();
            if name.is_some() {
                return Ok(name);
            }
        }
        let name: Option<String> = db
            .query_row(
                "SELECT name FROM services WHERE name = ?1",
                params![target],
                |r| r.get(0),
            )
            .ok();
        Ok(name)
    }

    /// Return every persisted service record, ordered by id.
    pub fn list(&self) -> Result<Vec<ServiceRecord>, ServiceError> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare("SELECT * FROM services ORDER BY id")?;
        let rows = stmt.query_map([], Self::row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Return one service record by name or id.
    pub fn describe(&self, target: &str) -> Result<ServiceRecord, ServiceError> {
        let name = self
            .resolve_target(target)?
            .ok_or_else(|| ServiceError::NotFound(target.to_string()))?;
        self.fetch(&name)?
            .ok_or_else(|| ServiceError::NotFound(target.to_string()))
    }

    /// Persist a service definition without spawning a child. Used by the
    /// snapshot resurrect path (Phase 4) and by tests that need a row in
    /// the table without the lifecycle side effects. If `name` already
    /// exists the row is updated in place; otherwise a fresh id is
    /// allocated.
    pub fn register_def(&self, def: ServiceDef) -> Result<ServiceRecord, ServiceError> {
        let id = match self.fetch(&def.name)? {
            Some(rec) => rec.id,
            None => self.next_id.fetch_add(1, Ordering::Relaxed),
        };
        self.upsert_def(&def, id)?;
        self.fetch(&def.name)?
            .ok_or(ServiceError::NotFound(def.name))
    }

    pub(crate) fn upsert_def(&self, def: &ServiceDef, id: u32) -> Result<(), ServiceError> {
        let db = self.db.lock().unwrap();
        let cmd_json = serde_json::to_string(&def.cmd).unwrap_or_else(|_| "[]".into());
        let env_json = serde_json::to_string(&def.env).unwrap_or_else(|_| "[]".into());
        db.execute(
            "INSERT INTO services
                (id, name, cmd, cwd, env, autorestart, max_restarts,
                 restart_delay_ms, kill_timeout_ms, min_uptime_ms, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'stopped')
             ON CONFLICT(name) DO UPDATE SET
                cmd = excluded.cmd, cwd = excluded.cwd, env = excluded.env,
                autorestart = excluded.autorestart,
                max_restarts = excluded.max_restarts,
                restart_delay_ms = excluded.restart_delay_ms,
                kill_timeout_ms = excluded.kill_timeout_ms,
                min_uptime_ms = excluded.min_uptime_ms",
            params![
                id,
                def.name,
                cmd_json,
                def.cwd,
                env_json,
                def.autorestart as i64,
                def.max_restarts,
                def.restart_delay_ms,
                def.kill_timeout_ms,
                def.min_uptime_ms,
            ],
        )?;
        Ok(())
    }

    fn set_status(&self, name: &str, status: ServiceStatus, pid: u32) -> Result<(), ServiceError> {
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE services SET status = ?2, pid = ?3 WHERE name = ?1",
            params![name, status.as_str(), pid],
        )?;
        Ok(())
    }

    fn mark_started(&self, name: &str, pid: u32, ts: f64) -> Result<(), ServiceError> {
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE services SET status = 'online', pid = ?2, last_started_at = ?3 \
             WHERE name = ?1",
            params![name, pid, ts],
        )?;
        Ok(())
    }

    fn mark_exited(
        &self,
        name: &str,
        status: ServiceStatus,
        exit_code: i32,
        ts: f64,
    ) -> Result<(), ServiceError> {
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE services SET status = ?2, pid = 0, last_exit_code = ?3, \
             last_exited_at = ?4 WHERE name = ?1",
            params![name, status.as_str(), exit_code, ts],
        )?;
        Ok(())
    }

    fn bump_restart_count(&self, name: &str) -> Result<u32, ServiceError> {
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE services SET restart_count = restart_count + 1 WHERE name = ?1",
            params![name],
        )?;
        let count: u32 = db
            .query_row(
                "SELECT restart_count FROM services WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(count)
    }

    // -- Spawn / lifecycle ---------------------------------------------------

    /// Return the `(stdout_path, stderr_path)` for a service. The name is
    /// sanitized through the crate-private `sanitize_name` helper before being
    /// joined with the log directory — this is the defense against
    /// path-traversal for any handler that takes the service name from an RPC
    /// payload.
    pub fn log_paths(&self, name: &str) -> (PathBuf, PathBuf) {
        let safe = sanitize_name(name);
        (
            self.log_dir.join(format!("{safe}-out.log")),
            self.log_dir.join(format!("{safe}-err.log")),
        )
    }

    /// Read the tail of a service's `stdout` + `stderr` log files and return
    /// a single string with each line prefixed by `[stdout]` / `[stderr]`.
    ///
    /// - `lines` caps the per-stream tail; `0` falls back to `default_lines`.
    /// - The combined response body is hard-capped at `byte_budget` (very
    ///   long lines are truncated mid-line with a `…(truncated)` marker)
    ///   so a single noisy service can't blow the IPC response budget.
    /// - Missing log files (service never started) yield empty output, not
    ///   an error — the service definition may legitimately exist with no
    ///   incarnation yet.
    /// - Returns `NotFound` only when the target name doesn't resolve to a
    ///   registered service.
    pub fn read_logs(
        &self,
        target: &str,
        lines: u32,
        default_lines: u32,
        byte_budget: usize,
    ) -> Result<String, ServiceError> {
        let name = self
            .resolve_target(target)?
            .ok_or_else(|| ServiceError::NotFound(target.to_string()))?;
        let (out_path, err_path) = self.log_paths(&name);
        let effective_lines = if lines == 0 { default_lines } else { lines };

        let mut output = String::new();
        let mut remaining = byte_budget;
        append_tail(
            &out_path,
            "[stdout]",
            effective_lines,
            &mut output,
            &mut remaining,
        );
        append_tail(
            &err_path,
            "[stderr]",
            effective_lines,
            &mut output,
            &mut remaining,
        );
        Ok(output)
    }

    /// Truncate the `stdout` + `stderr` log files for `target` to zero bytes.
    /// The crate-private writer threads (`spawn_log_writer`) hold the files in
    /// append mode, so opening a fresh handle with `truncate(true)` re-zeros
    /// the file without disturbing the appender (the next append resumes at
    /// offset 0 on both Windows and POSIX).
    ///
    /// Returns `Ok(count)` where `count` is the number of services whose log
    /// pair was touched (truncated or already absent). `target == "all"`
    /// flushes every service; otherwise the single named service is flushed.
    /// A missing single-target resolves to `NotFound`.
    pub fn flush_logs(&self, target: &str) -> Result<u32, ServiceError> {
        if target == "all" || target.is_empty() {
            let names: Vec<String> = self.list()?.into_iter().map(|r| r.def.name).collect();
            let mut count = 0u32;
            for name in names {
                if self.truncate_log_pair(&name) {
                    count += 1;
                }
            }
            return Ok(count);
        }
        let name = self
            .resolve_target(target)?
            .ok_or_else(|| ServiceError::NotFound(target.to_string()))?;
        if self.truncate_log_pair(&name) {
            Ok(1)
        } else {
            Ok(0)
        }
    }

    /// Truncate both log files for `name`. Missing files are treated as
    /// "already empty" and still count as success — the operator's
    /// intent (`make these empty`) is satisfied either way.
    fn truncate_log_pair(&self, name: &str) -> bool {
        let (out_path, err_path) = self.log_paths(name);
        let out_ok = truncate_if_exists(&out_path);
        let err_ok = truncate_if_exists(&err_path);
        out_ok || err_ok
    }

    /// Spawn (or respawn) the child for `def` and register the live handle.
    /// Marks the service online and persists the pid. Does NOT bump the
    /// restart count (callers do that for restarts).
    pub(crate) fn spawn_child(&self, def: &ServiceDef) -> Result<u32, ServiceError> {
        if def.cmd.is_empty() {
            return Err(ServiceError::InvalidConfig("cmd must not be empty".into()));
        }

        let config = ProcessConfig {
            command: CommandSpec::Argv(def.cmd.clone()),
            cwd: if def.cwd.is_empty() {
                None
            } else {
                Some(PathBuf::from(&def.cwd))
            },
            env: if def.env.is_empty() {
                None
            } else {
                Some(def.env.clone())
            },
            capture: true,
            stderr_mode: StderrMode::Pipe,
            creationflags: None,
            create_process_group: true,
            stdin_mode: StdinMode::Piped,
            nice: None,
        };
        let process = Arc::new(NativeProcess::new(config));
        process
            .start()
            .map_err(|e| ServiceError::Spawn(e.to_string()))?;
        let pid = process.pid().unwrap_or(0);

        let (out_path, err_path) = self.log_paths(&def.name);
        let log_shutdown = Arc::new(AtomicBool::new(false));

        let handles = vec![
            spawn_log_writer(
                Arc::clone(&process),
                StreamKind::Stdout,
                out_path,
                Arc::clone(&log_shutdown),
            ),
            spawn_log_writer(
                Arc::clone(&process),
                StreamKind::Stderr,
                err_path,
                Arc::clone(&log_shutdown),
            ),
        ];

        let owned = Arc::new(OwnedService {
            process,
            started_at: Instant::now(),
            intentional_stop: Arc::new(AtomicBool::new(false)),
            log_shutdown,
            reader_threads: Mutex::new(handles),
        });
        self.live.lock().unwrap().insert(def.name.clone(), owned);

        self.mark_started(&def.name, pid, unix_now())?;
        Ok(pid)
    }

    /// `runpm start`: create (or update) a service definition and launch it.
    pub fn start(&self, def: ServiceDef) -> Result<ServiceRecord, ServiceError> {
        if def.name.is_empty() {
            return Err(ServiceError::InvalidConfig("name must not be empty".into()));
        }
        if def.cmd.is_empty() {
            return Err(ServiceError::InvalidConfig("cmd must not be empty".into()));
        }

        // Already running? Reject — operators should `restart`.
        if let Some(existing) = self.fetch(&def.name)? {
            if existing.status == ServiceStatus::Online && self.is_live(&def.name) {
                return Err(ServiceError::AlreadyExists(def.name));
            }
        }

        let id = match self.fetch(&def.name)? {
            Some(rec) => rec.id,
            None => self.next_id.fetch_add(1, Ordering::Relaxed),
        };
        self.upsert_def(&def, id)?;
        // Reset the restart counter for a fresh start.
        {
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE services SET restart_count = 0 WHERE name = ?1",
                params![def.name],
            )?;
        }
        self.spawn_child(&def)?;
        self.fetch(&def.name)?
            .ok_or(ServiceError::NotFound(def.name))
    }

    pub(crate) fn is_live(&self, name: &str) -> bool {
        self.live
            .lock()
            .unwrap()
            .get(name)
            .map(|s| s.process.poll().ok().flatten().is_none())
            .unwrap_or(false)
    }

    /// Terminate the live child for `name` (if any) and mark it stopped.
    /// `intentional` suppresses the supervisor's auto-restart.
    fn stop_one(&self, name: &str, mark_status: ServiceStatus) -> bool {
        let owned = self.live.lock().unwrap().remove(name);
        let Some(owned) = owned else {
            // Not live; still flip the persisted status.
            let _ = self.set_status(name, mark_status, 0);
            return false;
        };
        owned.intentional_stop.store(true, Ordering::Release);
        let grace = self
            .fetch(name)
            .ok()
            .flatten()
            .map(|r| r.def.kill_grace())
            .unwrap_or_else(|| Duration::from_secs(3));
        // Soft signal, then hard kill within the grace window.
        let _ = owned.process.terminate_group_soft();
        if owned.process.wait(Some(grace)).is_err() {
            let _ = owned.process.kill();
        }
        owned.signal_log_shutdown();
        let _ = self.mark_exited(
            name,
            mark_status,
            owned.process.poll().ok().flatten().unwrap_or(0),
            unix_now(),
        );
        true
    }

    /// `runpm stop`: stop the targeted service(s). Returns the count stopped.
    pub fn stop(&self, target: &str) -> Result<u32, ServiceError> {
        let names = self.targets(target)?;
        let mut count = 0;
        for name in names {
            if self.stop_one(&name, ServiceStatus::Stopped) {
                count += 1;
            }
        }
        Ok(count)
    }

    /// `runpm restart`: stop then start the targeted service(s), bumping the
    /// restart count. Returns the count restarted.
    pub fn restart(&self, target: &str) -> Result<u32, ServiceError> {
        let names = self.targets(target)?;
        let mut count = 0;
        for name in &names {
            let Some(rec) = self.fetch(name)? else {
                continue;
            };
            self.stop_one(name, ServiceStatus::Stopped);
            self.bump_restart_count(name)?;
            self.spawn_child(&rec.def)?;
            count += 1;
        }
        Ok(count)
    }

    /// `runpm delete`: stop (if running) and remove the targeted service(s).
    pub fn delete(&self, target: &str) -> Result<u32, ServiceError> {
        let names = self.targets(target)?;
        let mut count = 0;
        for name in &names {
            self.stop_one(name, ServiceStatus::Stopped);
            let db = self.db.lock().unwrap();
            let removed = db.execute("DELETE FROM services WHERE name = ?1", params![name])?;
            if removed > 0 {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Resolve a target ("all", a name, or an id) to a concrete list of
    /// service names. Errors if a non-"all" target matches nothing.
    fn targets(&self, target: &str) -> Result<Vec<String>, ServiceError> {
        if target == "all" {
            return Ok(self.list()?.into_iter().map(|r| r.def.name).collect());
        }
        match self.resolve_target(target)? {
            Some(name) => Ok(vec![name]),
            None => Err(ServiceError::NotFound(target.to_string())),
        }
    }

    /// Stop every live child without restart. Called on daemon shutdown.
    pub fn shutdown_all(&self) {
        let names: Vec<String> = self.live.lock().unwrap().keys().cloned().collect();
        for name in names {
            self.stop_one(&name, ServiceStatus::Stopped);
        }
    }

    // -- Supervision ---------------------------------------------------------

    /// One supervision tick: detect children that exited and apply the
    /// restart policy. Returns the number of restarts performed (test hook).
    pub fn supervise_tick(&self) -> usize {
        // Snapshot live names + their exit/uptime status while holding the
        // lock briefly.
        let exited: Vec<(String, i32, Duration, bool)> = {
            let live = self.live.lock().unwrap();
            live.iter()
                .filter_map(|(name, owned)| {
                    let code = owned.process.poll().ok().flatten()?;
                    Some((
                        name.clone(),
                        code,
                        owned.started_at.elapsed(),
                        owned.intentional_stop.load(Ordering::Acquire),
                    ))
                })
                .collect()
        };

        let mut restarts = 0;
        for (name, code, uptime, intentional) in exited {
            // Drop the dead handle.
            if let Some(owned) = self.live.lock().unwrap().remove(&name) {
                owned.signal_log_shutdown();
            }
            if intentional {
                // Operator-initiated stop already recorded the exit.
                continue;
            }
            let Some(rec) = self.fetch(&name).ok().flatten() else {
                continue;
            };
            let policy = RestartPolicy::from_config(&rec.def);

            if !policy.autorestart {
                let _ = self.mark_exited(&name, ServiceStatus::Stopped, code, unix_now());
                info!(service = %name, code, "service exited (autorestart disabled)");
                continue;
            }

            // A service that stayed up long enough resets the rapid-crash
            // counter; otherwise this counts as a rapid crash.
            if uptime >= policy.min_uptime {
                let db = self.db.lock().unwrap();
                let _ = db.execute(
                    "UPDATE services SET restart_count = 0 WHERE name = ?1",
                    params![name],
                );
            }

            let crashes = self.bump_restart_count(&name).unwrap_or(0);
            if policy.max_restarts != 0 && crashes > policy.max_restarts {
                let _ = self.mark_exited(&name, ServiceStatus::Errored, code, unix_now());
                warn!(
                    service = %name,
                    crashes,
                    max = policy.max_restarts,
                    "service crashed too many times; marking errored"
                );
                continue;
            }

            let delay = policy.backoff_for(crashes.saturating_sub(1));
            debug!(service = %name, code, crashes, ?delay, "restarting service after backoff");
            if !delay.is_zero() {
                thread::sleep(delay);
            }
            match self.spawn_child(&rec.def) {
                Ok(pid) => {
                    info!(service = %name, pid, crashes, "service restarted");
                    restarts += 1;
                }
                Err(e) => {
                    let _ = self.mark_exited(&name, ServiceStatus::Errored, code, unix_now());
                    warn!(service = %name, error = %e, "failed to restart service");
                }
            }
        }
        restarts
    }
}

// ---------------------------------------------------------------------------
// Background supervisor task
// ---------------------------------------------------------------------------

/// Daemon-owned background task: ticks the supervisor on a fixed interval,
/// restarting crashed services per their policy, until shutdown.
pub async fn supervisor_loop(state: Arc<crate::daemon::handlers::DaemonState>, interval_secs: u64) {
    let mut shutdown_rx = state.shutdown_tx.subscribe();
    let interval = Duration::from_secs(interval_secs.max(1));
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let svc_state = Arc::clone(&state);
                let result = tokio::task::spawn_blocking(move || {
                    svc_state.services.supervise_tick()
                })
                .await;
                if let Err(e) = result {
                    warn!("supervisor tick panicked: {e}");
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("supervisor shutting down");
                    state.services.shutdown_all();
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Spawn a thread that drains one stream of a child into a log file,
/// appending a newline per line (mirroring [`crate::daemon::pipe_sessions`]).
fn spawn_log_writer(
    process: Arc<NativeProcess>,
    stream: StreamKind,
    path: PathBuf,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to open service log file");
                return;
            }
        };
        loop {
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            match process.read_stream(stream, Some(Duration::from_millis(100))) {
                ReadStatus::Line(mut bytes) => {
                    bytes.push(b'\n');
                    let _ = file.write_all(&bytes);
                    let _ = file.flush();
                }
                ReadStatus::Timeout => {}
                ReadStatus::Eof => break,
            }
        }
    })
}

/// Per-line cap when reading a log tail — anything longer is truncated with a
/// trailing marker so one runaway line can't blow the IPC budget.
const MAX_LOG_LINE_BYTES: usize = 4096;

/// Read the last `lines` lines from `path` and append them to `output`,
/// each tagged with `prefix`. Respects a remaining-byte budget so the
/// combined response can't exceed the caller's cap. Missing files are a
/// silent no-op — `service_logs` returns empty rather than erroring out
/// when a service has never started.
fn append_tail(path: &Path, prefix: &str, lines: u32, output: &mut String, remaining: &mut usize) {
    if *remaining == 0 {
        return;
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => return,
    };

    // Split on '\n' while tolerating a trailing empty line from a
    // newline-terminated file.
    let text = String::from_utf8_lossy(&bytes);
    let mut all: Vec<&str> = text.split('\n').collect();
    if all.last().map(|s| s.is_empty()).unwrap_or(false) {
        all.pop();
    }
    let start = all.len().saturating_sub(lines as usize);
    // Marker appended to over-long lines.
    const TRUNC_MARK: &str = "...(truncated)";
    for line in &all[start..] {
        if *remaining == 0 {
            break;
        }
        // Frame = "<prefix> <body>\n". Need at least one body byte to be
        // worth emitting.
        let frame_overhead = prefix.len() + 2;
        if *remaining <= frame_overhead {
            break;
        }
        let body_room = (*remaining - frame_overhead).min(MAX_LOG_LINE_BYTES);
        let pre_len = output.len();
        output.push_str(prefix);
        output.push(' ');
        if line.len() > body_room {
            let avail = body_room.saturating_sub(TRUNC_MARK.len());
            // Snap to a char boundary so the resulting slice is valid UTF-8.
            let mut cut = avail.min(line.len());
            while cut > 0 && !line.is_char_boundary(cut) {
                cut -= 1;
            }
            output.push_str(&line[..cut]);
            output.push_str(TRUNC_MARK);
        } else {
            output.push_str(line);
        }
        output.push('\n');
        let wrote = output.len() - pre_len;
        *remaining = remaining.saturating_sub(wrote);
    }
}

/// Open `path` with `truncate(true)`, which on POSIX and Windows alike
/// shrinks the file to zero bytes from a fresh handle without disturbing
/// the long-lived append-mode writer thread. Missing files are treated as
/// "already zero" — a successful no-op.
fn truncate_if_exists(path: &Path) -> bool {
    match std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to truncate service log file");
            false
        }
    }
}

/// Make a service name safe for use as a log filename component.
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn registry() -> (ServiceRegistry, TempDir) {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("svc.sqlite3");
        let logs = tmp.path().join("services");
        let reg = ServiceRegistry::open(&db, logs).unwrap();
        (reg, tmp)
    }

    /// A command that exits successfully after a tiny sleep — cross-platform.
    fn short_lived_cmd() -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd".to_string(), "/C".to_string(), "exit 0".to_string()]
        }
        #[cfg(not(windows))]
        {
            vec!["true".to_string()]
        }
    }

    /// A command that runs ~forever so it stays online for lifecycle tests.
    fn long_lived_cmd() -> Vec<String> {
        #[cfg(windows)]
        {
            // ping loopback 300 times ~= 300s; killed by the test.
            vec![
                "cmd".to_string(),
                "/C".to_string(),
                "ping -n 300 127.0.0.1 > NUL".to_string(),
            ]
        }
        #[cfg(not(windows))]
        {
            vec!["sleep".to_string(), "300".to_string()]
        }
    }

    fn def(name: &str, cmd: Vec<String>) -> ServiceDef {
        ServiceDef {
            name: name.to_string(),
            cmd,
            cwd: String::new(),
            env: Vec::new(),
            autorestart: false,
            max_restarts: 0,
            restart_delay_ms: 0,
            kill_timeout_ms: 500,
            min_uptime_ms: 0,
        }
    }

    #[test]
    fn table_crud_roundtrip() {
        let (reg, _tmp) = registry();
        let mut d = def("crud", short_lived_cmd());
        d.autorestart = false;
        // Persist without spawning by using upsert + describe paths.
        reg.upsert_def(&d, 1).unwrap();
        let got = reg.describe("crud").unwrap();
        assert_eq!(got.def.name, "crud");
        assert_eq!(got.def.cmd, short_lived_cmd());
        assert_eq!(got.status, ServiceStatus::Stopped);

        // List sees it.
        assert_eq!(reg.list().unwrap().len(), 1);

        // Resolve by id.
        let by_id = reg.describe(&got.id.to_string()).unwrap();
        assert_eq!(by_id.def.name, "crud");

        // Delete removes it.
        assert_eq!(reg.delete("crud").unwrap(), 1);
        assert!(reg.describe("crud").is_err());
    }

    #[test]
    fn start_list_stop_delete_cycle() {
        let (reg, _tmp) = registry();
        let rec = reg.start(def("svc1", long_lived_cmd())).unwrap();
        assert_eq!(rec.status, ServiceStatus::Online);
        assert!(rec.pid > 0);

        let all = reg.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, ServiceStatus::Online);

        // Starting again while online is rejected.
        assert!(matches!(
            reg.start(def("svc1", long_lived_cmd())),
            Err(ServiceError::AlreadyExists(_))
        ));

        assert_eq!(reg.stop("svc1").unwrap(), 1);
        assert_eq!(reg.describe("svc1").unwrap().status, ServiceStatus::Stopped);

        assert_eq!(reg.delete("svc1").unwrap(), 1);
        assert!(reg.describe("svc1").is_err());
    }

    #[test]
    fn restart_bumps_count() {
        let (reg, _tmp) = registry();
        reg.start(def("svc2", long_lived_cmd())).unwrap();
        assert_eq!(reg.describe("svc2").unwrap().restart_count, 0);

        reg.restart("svc2").unwrap();
        assert_eq!(reg.describe("svc2").unwrap().restart_count, 1);
        assert_eq!(reg.describe("svc2").unwrap().status, ServiceStatus::Online);

        reg.restart("svc2").unwrap();
        assert_eq!(reg.describe("svc2").unwrap().restart_count, 2);

        reg.stop("svc2").unwrap();
    }

    #[test]
    fn rapid_crash_transitions_to_errored() {
        let (reg, _tmp) = registry();
        let mut d = def("crasher", short_lived_cmd());
        d.autorestart = true;
        d.max_restarts = 3;
        d.restart_delay_ms = 1; // keep the test fast
        d.min_uptime_ms = 60_000; // never long enough to reset
        reg.start(d).unwrap();

        // The child exits almost immediately. Drive supervision until the
        // service is marked errored (bounded loop, fast fixture).
        let mut errored = false;
        for _ in 0..200 {
            reg.supervise_tick();
            let rec = reg.describe("crasher").unwrap();
            if rec.status == ServiceStatus::Errored {
                errored = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(errored, "service should have transitioned to errored");
        let rec = reg.describe("crasher").unwrap();
        assert!(rec.restart_count > rec.def.max_restarts);
        assert!(!reg.is_live("crasher"));
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let policy = RestartPolicy {
            autorestart: true,
            max_restarts: 0,
            base_delay: Duration::from_millis(100),
            min_uptime: Duration::from_secs(2),
        };
        assert_eq!(policy.backoff_for(0), Duration::from_millis(100));
        assert_eq!(policy.backoff_for(1), Duration::from_millis(200));
        assert_eq!(policy.backoff_for(2), Duration::from_millis(400));
        assert_eq!(policy.backoff_for(100), RestartPolicy::MAX_BACKOFF);
    }

    // -- Phase 3: logs + flush ---------------------------------------------

    /// Write `content` to the file at `path`, creating it if needed.
    fn write_log(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, content).expect("write log file");
    }

    /// Register a service definition without spawning a child. We only need
    /// the row to exist so `read_logs`/`flush_logs` can resolve the target.
    fn register_only(reg: &ServiceRegistry, name: &str, id: u32) {
        let d = def(name, short_lived_cmd());
        reg.upsert_def(&d, id).unwrap();
    }

    #[test]
    fn service_logs_returns_recent_lines_with_stream_prefix() {
        let (reg, _tmp) = registry();
        register_only(&reg, "logsvc", 1);
        let (out, err) = reg.log_paths("logsvc");
        // 5 stdout lines + 5 stderr lines.
        let out_body = (1..=5)
            .map(|i| format!("out-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let err_body = (1..=5)
            .map(|i| format!("err-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        write_log(&out, &format!("{out_body}\n"));
        write_log(&err, &format!("{err_body}\n"));

        let txt = reg.read_logs("logsvc", 10, 100, 64 * 1024).unwrap();

        for i in 1..=5 {
            assert!(
                txt.contains(&format!("[stdout] out-{i}")),
                "missing [stdout] out-{i} in {txt}"
            );
            assert!(
                txt.contains(&format!("[stderr] err-{i}")),
                "missing [stderr] err-{i} in {txt}"
            );
        }
    }

    #[test]
    fn service_logs_empty_when_files_missing() {
        let (reg, _tmp) = registry();
        register_only(&reg, "neverran", 2);
        let txt = reg.read_logs("neverran", 100, 100, 64 * 1024).unwrap();
        assert!(txt.is_empty(), "expected empty body, got {txt:?}");
    }

    #[test]
    fn service_logs_unknown_service_is_not_found() {
        let (reg, _tmp) = registry();
        let res = reg.read_logs("nope", 10, 100, 64 * 1024);
        assert!(matches!(res, Err(ServiceError::NotFound(_))));
    }

    #[test]
    fn service_logs_caps_response_size() {
        let (reg, _tmp) = registry();
        register_only(&reg, "bigsvc", 3);
        let (out, _err) = reg.log_paths("bigsvc");
        // ~200 KB of body in a single huge line — well above the 64 KiB cap.
        let huge = "x".repeat(200 * 1024);
        write_log(&out, &format!("{huge}\n"));

        let txt = reg.read_logs("bigsvc", 10, 100, 64 * 1024).unwrap();

        // Response is bounded near the cap.
        assert!(
            txt.len() <= 64 * 1024,
            "response exceeded budget: {} bytes",
            txt.len()
        );
        // Truncation marker must appear on the over-long line.
        assert!(
            txt.contains("...(truncated)"),
            "expected truncation marker in {}",
            &txt[..200.min(txt.len())]
        );
    }

    #[test]
    fn service_flush_zeros_log_files() {
        let (reg, _tmp) = registry();
        register_only(&reg, "tobeflushed", 4);
        let (out, err) = reg.log_paths("tobeflushed");
        write_log(&out, "hello-out\n");
        write_log(&err, "hello-err\n");

        let count = reg.flush_logs("tobeflushed").unwrap();
        assert_eq!(count, 1, "exactly one service should be flushed");

        let out_len = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(u64::MAX);
        let err_len = std::fs::metadata(&err).map(|m| m.len()).unwrap_or(u64::MAX);
        assert_eq!(out_len, 0, "{} should be 0 bytes", out.display());
        assert_eq!(err_len, 0, "{} should be 0 bytes", err.display());
    }

    #[test]
    fn service_flush_all_targets_every_service() {
        let (reg, _tmp) = registry();
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            register_only(&reg, name, (i + 1) as u32);
            let (out, err) = reg.log_paths(name);
            write_log(&out, "x\n");
            write_log(&err, "y\n");
        }
        let count = reg.flush_logs("all").unwrap();
        assert_eq!(count, 3, "all three services should be flushed");
    }

    #[test]
    fn service_flush_unknown_single_target_is_not_found() {
        let (reg, _tmp) = registry();
        let res = reg.flush_logs("nope");
        assert!(matches!(res, Err(ServiceError::NotFound(_))));
    }

    #[test]
    fn service_log_paths_resist_traversal() {
        // The sanitizer replaces directory separators (`/`, `\\`) with
        // underscores so the resulting filename can never escape log_dir.
        // A leading `..` is allowed as filename characters but the lack of
        // separators means the result is a single filename component
        // joined under `log_dir` — not a parent-relative path.
        let (reg, tmp) = registry();
        let (out, err) = reg.log_paths("../../../etc/passwd");
        let log_dir = tmp.path().join("services");
        assert_eq!(
            out.parent().unwrap(),
            log_dir,
            "out log must live directly under the log dir, got {}",
            out.display()
        );
        assert_eq!(
            err.parent().unwrap(),
            log_dir,
            "err log must live directly under the log dir, got {}",
            err.display()
        );
        // And the filename must contain no path separators.
        let out_name = out.file_name().unwrap().to_string_lossy();
        let err_name = err.file_name().unwrap().to_string_lossy();
        assert!(!out_name.contains('/') && !out_name.contains('\\'));
        assert!(!err_name.contains('/') && !err_name.contains('\\'));
    }
}
