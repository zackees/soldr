//! Snapshot save/resurrect for the runpm `ServiceRegistry` (Phase 4 — #427).
//!
//! The snapshot is a single JSON file (`services.snapshot.json`) written
//! next to the SQLite registry file. Writes are atomic via temp-file +
//! `fsync` + rename. Resurrect uses `INSERT OR REPLACE` semantics so it is
//! safe to call repeatedly: a second call updates definitions in place
//! rather than colliding on the unique-name constraint.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::daemon::services::{
    ServiceDef, ServiceError, ServiceRecord, ServiceRegistry, ServiceStatus,
};

/// Snapshot file name written by `save` and read by `resurrect`. Always
/// lives next to the SQLite registry file inside the per-scope local dir.
pub const SNAPSHOT_FILE_NAME: &str = "services.snapshot.json";

/// Current snapshot format version. Bumped only when the on-disk shape
/// changes incompatibly; readers accept exactly this number.
pub const SNAPSHOT_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Serde DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SnapshotEnvelope {
    pub version: u32,
    pub saved_at_ms: u64,
    pub services: Vec<SnapshotService>,
}

/// One row from the `services` SQLite table, flat-serialized as JSON.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SnapshotService {
    pub id: u32,
    pub name: String,
    pub cmd: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub autorestart: bool,
    pub max_restarts: u32,
    pub restart_delay_ms: u32,
    pub kill_timeout_ms: u32,
    pub min_uptime_ms: u32,
    pub status: String,
    pub pid: u32,
    pub restart_count: u32,
    pub last_started_at: f64,
    pub last_exited_at: f64,
    pub last_exit_code: i32,
}

impl SnapshotService {
    pub fn from_record(rec: &ServiceRecord) -> Self {
        Self {
            id: rec.id,
            name: rec.def.name.clone(),
            cmd: rec.def.cmd.clone(),
            cwd: rec.def.cwd.clone(),
            env: rec.def.env.clone(),
            autorestart: rec.def.autorestart,
            max_restarts: rec.def.max_restarts,
            restart_delay_ms: rec.def.restart_delay_ms,
            kill_timeout_ms: rec.def.kill_timeout_ms,
            min_uptime_ms: rec.def.min_uptime_ms,
            status: rec.status.as_str().to_string(),
            pid: rec.pid,
            restart_count: rec.restart_count,
            last_started_at: rec.last_started_at,
            last_exited_at: rec.last_exited_at,
            last_exit_code: rec.last_exit_code,
        }
    }

    pub fn to_def(&self) -> ServiceDef {
        ServiceDef {
            name: self.name.clone(),
            cmd: self.cmd.clone(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            autorestart: self.autorestart,
            max_restarts: self.max_restarts,
            restart_delay_ms: self.restart_delay_ms,
            kill_timeout_ms: self.kill_timeout_ms,
            min_uptime_ms: self.min_uptime_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// Save
// ---------------------------------------------------------------------------

/// Persist the current `services` table to `services.snapshot.json`. Writes
/// are atomic via temp-file + `fsync` + rename so a crash partway through
/// never leaves a half-written snapshot in place.
///
/// Returns the (absolute) snapshot path that was written and the number of
/// service rows it contains.
pub fn save_snapshot(reg: &ServiceRegistry) -> Result<(PathBuf, u32), ServiceError> {
    use std::io::Write;

    let services: Vec<SnapshotService> = reg
        .list()?
        .iter()
        .map(SnapshotService::from_record)
        .collect();
    let saved_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0) as u64;
    let count = services.len() as u32;
    let envelope = SnapshotEnvelope {
        version: SNAPSHOT_VERSION,
        saved_at_ms,
        services,
    };
    let json = serde_json::to_string_pretty(&envelope)
        .map_err(|e| ServiceError::Db(format!("snapshot serialize failed: {e}")))?;

    let snapshot_path = reg.snapshot_path.clone();
    if let Some(parent) = snapshot_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ServiceError::Db(format!("snapshot mkdir failed: {e}")))?;
    }
    let tmp = snapshot_path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| {
                ServiceError::Db(format!("snapshot tmp open failed ({}): {e}", tmp.display()))
            })?;
        f.write_all(json.as_bytes())
            .map_err(|e| ServiceError::Db(format!("snapshot write failed: {e}")))?;
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, &snapshot_path).map_err(|e| {
        ServiceError::Db(format!(
            "snapshot rename failed ({} -> {}): {e}",
            tmp.display(),
            snapshot_path.display()
        ))
    })?;

    info!(path = %snapshot_path.display(), count, "service snapshot saved");
    Ok((snapshot_path, count))
}

// ---------------------------------------------------------------------------
// Resurrect
// ---------------------------------------------------------------------------

/// Restore service definitions from `services.snapshot.json` and re-launch
/// every service that was `online` at snapshot time. Returns
/// `(restored_count, restarted_count)`.
pub fn resurrect_from_snapshot(reg: &ServiceRegistry) -> Result<(u32, u32), ServiceError> {
    let snapshot_path = reg.snapshot_path.clone();
    let bytes = match std::fs::read(&snapshot_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ServiceError::NotFound(format!(
                "no snapshot found at {}",
                snapshot_path.display()
            )));
        }
        Err(e) => {
            return Err(ServiceError::Db(format!(
                "snapshot read failed ({}): {e}",
                snapshot_path.display()
            )));
        }
    };
    let envelope: SnapshotEnvelope = serde_json::from_slice(&bytes).map_err(|e| {
        ServiceError::InvalidConfig(format!(
            "snapshot at {} is not valid JSON: {e}",
            snapshot_path.display()
        ))
    })?;
    if envelope.version != SNAPSHOT_VERSION {
        return Err(ServiceError::InvalidConfig(format!(
            "snapshot version {} is not supported (expected {})",
            envelope.version, SNAPSHOT_VERSION
        )));
    }

    let mut restored = 0u32;
    let mut restarted = 0u32;
    for entry in envelope.services {
        let def = entry.to_def();
        let id = if entry.id == 0 {
            reg.next_id.fetch_add(1, Ordering::Relaxed)
        } else {
            reg.next_id.fetch_max(entry.id + 1, Ordering::Relaxed);
            entry.id
        };
        if let Err(e) = reg.upsert_def(&def, id) {
            warn!(service = %def.name, error = %e, "failed to restore service def");
            continue;
        }
        restored += 1;

        if entry.status == ServiceStatus::Online.as_str() && !reg.is_live(&def.name) {
            if let Err(e) = reg.spawn_child(&def) {
                warn!(
                    service = %def.name,
                    error = %e,
                    "failed to restart resurrected service"
                );
                continue;
            }
            restarted += 1;
        }
    }

    info!(
        path = %snapshot_path.display(),
        restored,
        restarted,
        "service snapshot resurrected"
    );
    Ok((restored, restarted))
}
