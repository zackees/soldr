//! Daemon-side redb tables for build session correlation and the
//! linked-zccache PID. Opens `~/.soldr/state.redb` per call and drops
//! the handle on return — redb refuses concurrent multi-process opens
//! and the wrapper / GC tools open the same file directly.
//!
//! Tables live alongside the existing `target_registry_targets`:
//! - `daemon_builds`   : u64 session_id  → bincoded BuildRecord
//! - `daemon_events`   : u64 event_id    → bincoded Event
//! - `daemon_meta`     : &str meta_key   → u64 value (next event id;
//!                                          linked zccache PID; etc.)

use crate::cache_lib::target_registry::RegistryError;
use crate::daemon::protocol::BuildRecord;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const BUILDS: TableDefinition<u64, &[u8]> = TableDefinition::new("daemon_builds");
const EVENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("daemon_events");
const META: TableDefinition<&str, u64> = TableDefinition::new("daemon_meta");

const META_NEXT_EVENT_ID: &str = "next_event_id";
const META_LINKED_ZCCACHE_PID: &str = "linked_zccache_pid";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EventKind {
    SessionStart,
    SessionEnd,
    CompileStart,
    CompileEnd,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub ts_ms: i64,
    pub session_id: Option<u64>,
    pub kind: EventKind,
    pub crate_name: Option<String>,
    pub duration_us: Option<u64>,
    pub target_dir: Option<String>,
    pub exit_code: Option<i32>,
}

fn bincode_err(e: bincode::Error) -> RegistryError {
    RegistryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn open_db(path: &Path) -> Result<Database, RegistryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = Database::builder().create(path)?;
    Ok(db)
}

fn init_tables(db: &Database) -> Result<(), RegistryError> {
    let txn = db.begin_write()?;
    {
        let _ = txn.open_table(BUILDS)?;
        let _ = txn.open_table(EVENTS)?;
        let _ = txn.open_table(META)?;
    }
    txn.commit()?;
    Ok(())
}

/// Open + initialize the daemon tables in the shared state.redb. Idempotent.
pub fn ensure_initialized(db_path: &Path) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)
}

fn next_event_id(db: &Database) -> Result<u64, RegistryError> {
    let txn = db.begin_write()?;
    let next = {
        let mut meta = txn.open_table(META)?;
        let current = meta
            .get(META_NEXT_EVENT_ID)?
            .map(|v| v.value())
            .unwrap_or(1);
        let next = current.saturating_add(1);
        meta.insert(META_NEXT_EVENT_ID, &next)?;
        current
    };
    txn.commit()?;
    Ok(next)
}

pub fn append_event(db_path: &Path, event: &Event) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let id = next_event_id(&db)?;
    let bytes = bincode::serialize(event).map_err(bincode_err)?;
    let txn = db.begin_write()?;
    {
        let mut events = txn.open_table(EVENTS)?;
        events.insert(id, bytes.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

pub fn upsert_build(db_path: &Path, record: &BuildRecord) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let bytes = bincode::serialize(record).map_err(bincode_err)?;
    let txn = db.begin_write()?;
    {
        let mut builds = txn.open_table(BUILDS)?;
        builds.insert(record.session_id, bytes.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

pub fn get_build(db_path: &Path, session_id: u64) -> Result<Option<BuildRecord>, RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let txn = db.begin_read()?;
    let builds = txn.open_table(BUILDS)?;
    let Some(row) = builds.get(session_id)? else {
        return Ok(None);
    };
    let record: BuildRecord = bincode::deserialize(row.value()).map_err(bincode_err)?;
    Ok(Some(record))
}

pub fn list_builds(
    db_path: &Path,
    limit: u32,
    since_ms: Option<i64>,
) -> Result<Vec<BuildRecord>, RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let txn = db.begin_read()?;
    let builds = txn.open_table(BUILDS)?;
    let mut rows: Vec<BuildRecord> = Vec::new();
    for entry in builds.iter()? {
        let (_, v) = entry?;
        let record: BuildRecord = bincode::deserialize(v.value()).map_err(bincode_err)?;
        if let Some(cutoff) = since_ms {
            if record.started_at_ms < cutoff {
                continue;
            }
        }
        rows.push(record);
    }
    rows.sort_by(|a, b| b.started_at_ms.cmp(&a.started_at_ms));
    rows.truncate(limit as usize);
    Ok(rows)
}

pub fn list_slow_builds(
    db_path: &Path,
    threshold_ms: u64,
    limit: u32,
) -> Result<Vec<BuildRecord>, RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let txn = db.begin_read()?;
    let builds = txn.open_table(BUILDS)?;
    let mut rows: Vec<BuildRecord> = Vec::new();
    for entry in builds.iter()? {
        let (_, v) = entry?;
        let record: BuildRecord = bincode::deserialize(v.value()).map_err(bincode_err)?;
        if record.total_wall_ms.unwrap_or(0) >= threshold_ms {
            rows.push(record);
        }
    }
    rows.sort_by(|a, b| {
        b.total_wall_ms
            .unwrap_or(0)
            .cmp(&a.total_wall_ms.unwrap_or(0))
    });
    rows.truncate(limit as usize);
    Ok(rows)
}

/// Walk events for `session_id`, returning `(crate_count, slowest_us, slowest_name)`.
pub fn aggregate_session(
    db_path: &Path,
    session_id: u64,
) -> Result<(u32, Option<u64>, Option<String>), RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let txn = db.begin_read()?;
    let events = txn.open_table(EVENTS)?;
    let mut count: u32 = 0;
    let mut slowest_us: Option<u64> = None;
    let mut slowest_name: Option<String> = None;
    for entry in events.iter()? {
        let (_, v) = entry?;
        let event: Event = bincode::deserialize(v.value()).map_err(bincode_err)?;
        if event.session_id != Some(session_id) {
            continue;
        }
        if matches!(event.kind, EventKind::CompileStart | EventKind::CompileEnd) {
            count += 1;
        }
        if let Some(d) = event.duration_us {
            if d > slowest_us.unwrap_or(0) {
                slowest_us = Some(d);
                slowest_name = event.crate_name.clone();
            }
        }
    }
    Ok((count, slowest_us, slowest_name))
}

/// Set the linked zccache PID. None clears it.
pub fn set_linked_zccache_pid(db_path: &Path, pid: Option<u32>) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let txn = db.begin_write()?;
    {
        let mut meta = txn.open_table(META)?;
        match pid {
            Some(p) => {
                meta.insert(META_LINKED_ZCCACHE_PID, &(p as u64))?;
            }
            None => {
                meta.remove(META_LINKED_ZCCACHE_PID)?;
            }
        }
    }
    txn.commit()?;
    Ok(())
}

pub fn get_linked_zccache_pid(db_path: &Path) -> Result<Option<u32>, RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let txn = db.begin_read()?;
    let meta = txn.open_table(META)?;
    Ok(meta.get(META_LINKED_ZCCACHE_PID)?.map(|v| v.value() as u32))
}

/// Delete `daemon_events` rows older than `cutoff_ms`. Returns the
/// number of rows removed.
pub fn prune_events_older_than(db_path: &Path, cutoff_ms: i64) -> Result<u64, RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let mut to_delete: Vec<u64> = Vec::new();
    {
        let txn = db.begin_read()?;
        let events = txn.open_table(EVENTS)?;
        for entry in events.iter()? {
            let (k, v) = entry?;
            let event: Event = bincode::deserialize(v.value()).map_err(bincode_err)?;
            if event.ts_ms < cutoff_ms {
                to_delete.push(k.value());
            }
        }
    }
    if to_delete.is_empty() {
        return Ok(0);
    }
    let txn = db.begin_write()?;
    let mut removed: u64 = 0;
    {
        let mut events = txn.open_table(EVENTS)?;
        for id in &to_delete {
            if events.remove(*id)?.is_some() {
                removed += 1;
            }
        }
    }
    txn.commit()?;
    Ok(removed)
}

/// Convenience wrapper used by integration tests and the CLI: derive
/// the db path from `SoldrPaths` once.
pub fn db_path(paths: &crate::core::SoldrPaths) -> PathBuf {
    crate::cache_lib::data_db_path(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn append_then_aggregate_counts_events_for_session() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("state.redb");
        let now = now_ms();
        append_event(
            &path,
            &Event {
                ts_ms: now,
                session_id: Some(42),
                kind: EventKind::CompileStart,
                crate_name: Some("foo".into()),
                duration_us: Some(100_000),
                target_dir: Some("/t".into()),
                exit_code: None,
            },
        )
        .expect("append");
        append_event(
            &path,
            &Event {
                ts_ms: now,
                session_id: Some(42),
                kind: EventKind::CompileEnd,
                crate_name: Some("bar".into()),
                duration_us: Some(250_000),
                target_dir: Some("/t".into()),
                exit_code: None,
            },
        )
        .expect("append");
        // Unrelated session should be ignored.
        append_event(
            &path,
            &Event {
                ts_ms: now,
                session_id: Some(7),
                kind: EventKind::CompileStart,
                crate_name: Some("zzz".into()),
                duration_us: Some(999_999_999),
                target_dir: None,
                exit_code: None,
            },
        )
        .expect("append");
        let (count, slowest_us, slowest_name) = aggregate_session(&path, 42).expect("aggregate");
        assert_eq!(count, 2);
        assert_eq!(slowest_us, Some(250_000));
        assert_eq!(slowest_name.as_deref(), Some("bar"));
    }

    #[test]
    fn list_slow_builds_filters_and_sorts() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("state.redb");
        for (sid, wall) in [(1u64, 100u64), (2, 5000), (3, 1500), (4, 7000)] {
            upsert_build(
                &path,
                &BuildRecord {
                    session_id: sid,
                    repo_root: "/r".into(),
                    started_at_ms: now_ms(),
                    ended_at_ms: Some(now_ms() + wall as i64),
                    exit_code: Some(0),
                    total_wall_ms: Some(wall),
                    crate_count: 1,
                    slowest_crate_us: None,
                    slowest_crate_name: None,
                },
            )
            .expect("upsert");
        }
        let slow = list_slow_builds(&path, 1000, 10).expect("slow");
        // walls >= 1000 ms: 5000, 1500, 7000 → sorted desc.
        assert_eq!(slow.len(), 3);
        assert_eq!(slow[0].total_wall_ms, Some(7000));
        assert_eq!(slow[1].total_wall_ms, Some(5000));
        assert_eq!(slow[2].total_wall_ms, Some(1500));
    }

    #[test]
    fn list_builds_returns_newest_first() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("state.redb");
        let base = now_ms();
        for (sid, ts) in [(10u64, base), (20, base + 100), (30, base + 200)] {
            upsert_build(
                &path,
                &BuildRecord {
                    session_id: sid,
                    repo_root: "/r".into(),
                    started_at_ms: ts,
                    ended_at_ms: None,
                    exit_code: None,
                    total_wall_ms: None,
                    crate_count: 0,
                    slowest_crate_us: None,
                    slowest_crate_name: None,
                },
            )
            .expect("upsert");
        }
        let list = list_builds(&path, 10, None).expect("list");
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].session_id, 30);
        assert_eq!(list[1].session_id, 20);
        assert_eq!(list[2].session_id, 10);
        // since_ms filter
        let filtered = list_builds(&path, 10, Some(base + 150)).expect("filtered");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session_id, 30);
        // limit
        let limited = list_builds(&path, 2, None).expect("limited");
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn linked_zccache_pid_round_trips() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("state.redb");
        assert_eq!(get_linked_zccache_pid(&path).expect("get"), None);
        set_linked_zccache_pid(&path, Some(12345)).expect("set");
        assert_eq!(get_linked_zccache_pid(&path).expect("get"), Some(12345));
        set_linked_zccache_pid(&path, None).expect("clear");
        assert_eq!(get_linked_zccache_pid(&path).expect("get"), None);
    }

    #[test]
    fn prune_events_drops_old_rows() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("state.redb");
        append_event(
            &path,
            &Event {
                ts_ms: 1_000,
                session_id: None,
                kind: EventKind::SessionStart,
                crate_name: None,
                duration_us: None,
                target_dir: None,
                exit_code: None,
            },
        )
        .expect("old");
        append_event(
            &path,
            &Event {
                ts_ms: 5_000,
                session_id: None,
                kind: EventKind::SessionEnd,
                crate_name: None,
                duration_us: None,
                target_dir: None,
                exit_code: None,
            },
        )
        .expect("fresh");
        let removed = prune_events_older_than(&path, 3_000).expect("prune");
        assert_eq!(removed, 1);
    }
}
