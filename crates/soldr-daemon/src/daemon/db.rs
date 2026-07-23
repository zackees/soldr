//! Daemon-side redb tables for build session correlation. Opens
//! `~/.soldr/state.redb` per call
//! and drops the handle on return — redb refuses concurrent multi-
//! process opens and the wrapper / GC tools open the same file directly.
//!
//! Tables live alongside the existing `target_registry_targets`:
//! - `daemon_builds`        : u64 session_id → tagged-byte BuildRecord
//! - `daemon_events`        : u64 event_id   → tagged-byte Event
//! - `daemon_meta`          : `&str` key     → u64 (next event id, ...)
//!
//! ## Serialization (issue #603 cleanup of #580)
//!
//! Every value in every table is `[0x01][prost body]` — no other shape
//! is accepted. Pre-#580 rows (raw bincode bytes) are dropped on-sight
//! by [`ensure_initialized`], which scans every table on each daemon
//! startup and removes any row that doesn't carry the prost tag. That
//! one-time migration is idempotent (subsequent runs find nothing to
//! drop) and cheap (the tables are small — single-digit-thousands of
//! rows at most). Losing build-history rows is acceptable: cache
//! contents on disk are untouched, only the per-build timing snapshots
//! are dropped.

use crate::cache_lib::redb_lock::{open_state_db, StateDbHandle};
use crate::cache_lib::target_registry::RegistryError;
use crate::daemon::protocol::{BuildRecord, WireDecodeError};
use crate::daemon::wire::{self, prost_tagged_bytes, REDB_TAG_PROST};
use prost::Message;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::{Path, PathBuf};

fn wire_err(e: WireDecodeError) -> RegistryError {
    RegistryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn prost_decode_err(e: prost::DecodeError) -> RegistryError {
    RegistryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

const BUILDS: TableDefinition<u64, &[u8]> = TableDefinition::new("daemon_builds");
const EVENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("daemon_events");
const META: TableDefinition<&str, u64> = TableDefinition::new("daemon_meta");

const META_NEXT_EVENT_ID: &str = "next_event_id";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    SessionStart,
    SessionEnd,
    CompileStart,
    CompileEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub ts_ms: i64,
    pub session_id: Option<u64>,
    pub kind: EventKind,
    pub crate_name: Option<String>,
    pub duration_us: Option<u64>,
    pub target_dir: Option<String>,
    pub exit_code: Option<i32>,
}

/// Acquire the shared [`state_db_open_lock`] and open the redb file
/// under it. The returned [`StateDbHandle`] derefs to [`Database`] so
/// existing call sites that just chain `.begin_write()` / `.begin_read()`
/// keep working. Holding the lock for the full lifetime of the handle
/// is required: redb's per-file lock is only released on `Database`
/// drop, so a second opener overlapping us would error out with
/// `Database already open. Cannot acquire lock.` (#608).
fn open_db(path: &Path) -> Result<StateDbHandle, RegistryError> {
    Ok(open_state_db(path)?)
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

/// One-time migration sweep: walk every value-bearing table and drop
/// any row whose first byte is not [`REDB_TAG_PROST`]. Rows written by
/// pre-#580 daemons are bincode-encoded and start with whatever bincode
/// emits first (typically not `0x01`) — they're unreadable by the new
/// prost-only decoder, so we evict them rather than fail on read.
///
/// Idempotent. After the first daemon startup post-#603 the function
/// finds nothing to drop and returns instantly. Costs are bounded:
/// daemon registry tables hold thousands of rows at most.
fn migrate_drop_non_prost_rows(db: &Database) -> Result<(), RegistryError> {
    let mut builds_drop: Vec<u64> = Vec::new();
    let mut events_drop: Vec<u64> = Vec::new();
    {
        let txn = db.begin_read()?;
        let builds = txn.open_table(BUILDS)?;
        for entry in builds.iter()? {
            let (k, v) = entry?;
            if v.value().first().copied() != Some(REDB_TAG_PROST) {
                builds_drop.push(k.value());
            }
        }
        let events = txn.open_table(EVENTS)?;
        for entry in events.iter()? {
            let (k, v) = entry?;
            if v.value().first().copied() != Some(REDB_TAG_PROST) {
                events_drop.push(k.value());
            }
        }
    }
    if builds_drop.is_empty() && events_drop.is_empty() {
        return Ok(());
    }
    let txn = db.begin_write()?;
    {
        let mut builds = txn.open_table(BUILDS)?;
        for id in &builds_drop {
            builds.remove(*id)?;
        }
        let mut events = txn.open_table(EVENTS)?;
        for id in &events_drop {
            events.remove(*id)?;
        }
    }
    txn.commit()?;
    let dropped = builds_drop.len() + events_drop.len();
    if dropped > 0 {
        eprintln!(
            "soldr-daemon: dropped {dropped} pre-#580 redb rows during one-time format migration \
             (builds={}, events={})",
            builds_drop.len(),
            events_drop.len(),
        );
    }
    Ok(())
}

/// Open + initialize the daemon tables in the shared `state.redb`. Idempotent.
///
/// Also runs the [`migrate_drop_non_prost_rows`] one-time migration so a
/// daemon starting up against a `state.redb` written by a pre-#603 build
/// returns to a clean, fully-prost-encoded state.
pub fn ensure_initialized(db_path: &Path) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    migrate_drop_non_prost_rows(&db)?;
    Ok(())
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
    let bytes = prost_tagged_bytes(&wire::event_to_wire(event));
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
    let bytes = prost_tagged_bytes(&wire::build_record_to_wire(record));
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
    decode_build_row(row.value()).map(Some)
}

/// Clear archive payload paths after history retention removes the matching
/// session directory.  Build timing/cache metadata remains queryable; readers
/// see `None` instead of a dangling path (#1763).
pub fn mark_archives_unavailable(
    db_path: &Path,
    session_ids: &[u64],
) -> Result<u64, RegistryError> {
    if session_ids.is_empty() {
        return Ok(0);
    }
    let ids = session_ids
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let txn = db.begin_write()?;
    let mut updated = 0_u64;
    {
        let mut builds = txn.open_table(BUILDS)?;
        for id in ids {
            let Some(row) = builds.get(id)? else {
                continue;
            };
            let mut record = decode_build_row(row.value())?;
            drop(row);
            let Some(paths) = record.log_paths.as_mut() else {
                continue;
            };
            paths.archived_session_log_path = None;
            paths.archived_journal_path = None;
            paths.archived_session_stats_path = None;
            paths.archived_compile_journal_path = None;
            let bytes = prost_tagged_bytes(&wire::build_record_to_wire(&record));
            builds.insert(id, bytes.as_slice())?;
            updated += 1;
        }
    }
    txn.commit()?;
    Ok(updated)
}

/// Read-modify-write finalization of a BuildRecord in ONE redb open +
/// write txn (soldr#1536). The per-call [`open_db`] cost grows with the
/// db file size, so the session-end path avoids paying it twice for a
/// `get_build` + `upsert_build` pair.
pub fn finalize_build(
    db_path: &Path,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
    aggregate: (u32, Option<u64>, Option<String>),
) -> Result<BuildRecord, RegistryError> {
    let (crate_count, slowest_crate_us, slowest_crate_name) = aggregate;
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let txn = db.begin_write()?;
    let record = {
        let mut builds = txn.open_table(BUILDS)?;
        let existing = match builds.get(session_id)? {
            Some(row) => Some(decode_build_row(row.value())?),
            None => None,
        };
        let mut record = existing.unwrap_or(BuildRecord {
            session_id,
            repo_root: String::new(),
            started_at_ms: ended_at_ms,
            ended_at_ms: None,
            exit_code: None,
            total_wall_ms: None,
            crate_count: 0,
            slowest_crate_us: None,
            slowest_crate_name: None,
            cache_summary: None,
            log_paths: None,
            miss_reasons: Vec::new(),
        });
        record.ended_at_ms = Some(ended_at_ms);
        record.exit_code = Some(exit_code);
        record.total_wall_ms = Some((ended_at_ms - record.started_at_ms).max(0) as u64);
        record.crate_count = crate_count;
        record.slowest_crate_us = slowest_crate_us;
        record.slowest_crate_name = slowest_crate_name;
        let bytes = prost_tagged_bytes(&wire::build_record_to_wire(&record));
        builds.insert(session_id, bytes.as_slice())?;
        record
    };
    txn.commit()?;
    Ok(record)
}

/// Strict prost-only decoder. A row that doesn't carry the prost tag
/// byte surfaces an `InvalidData` error — the migration pass in
/// [`ensure_initialized`] guarantees no such rows exist post-startup,
/// so this branch only fires if the file is corrupted in flight.
fn decode_build_row(bytes: &[u8]) -> Result<BuildRecord, RegistryError> {
    let rest = strip_prost_tag(bytes)?;
    let wire = wire::proto::WireBuildRecord::decode(rest).map_err(prost_decode_err)?;
    Ok(wire::build_record_from_wire(wire))
}

fn decode_event_row(bytes: &[u8]) -> Result<Event, RegistryError> {
    let rest = strip_prost_tag(bytes)?;
    let wire = wire::proto::WireEvent::decode(rest).map_err(prost_decode_err)?;
    wire::event_from_wire(wire).map_err(wire_err)
}

fn strip_prost_tag(bytes: &[u8]) -> Result<&[u8], RegistryError> {
    match bytes.split_first() {
        Some((&REDB_TAG_PROST, rest)) => Ok(rest),
        _ => Err(RegistryError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "redb row missing prost tag byte (post-#603 daemon expects every value to start with 0x01)",
        ))),
    }
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
        let record = decode_build_row(v.value())?;
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
        let record = decode_build_row(v.value())?;
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

pub fn list_events_for_session(
    db_path: &Path,
    session_id: u64,
) -> Result<Vec<Event>, RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let txn = db.begin_read()?;
    let events = txn.open_table(EVENTS)?;
    let mut rows: Vec<Event> = Vec::new();
    for entry in events.iter()? {
        let (_, v) = entry?;
        let event = decode_event_row(v.value())?;
        if event.session_id == Some(session_id) {
            rows.push(event);
        }
    }
    rows.sort_by(|a, b| a.ts_ms.cmp(&b.ts_ms));
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
    let mut start_count: u32 = 0;
    let mut end_count: u32 = 0;
    let mut slowest_us: Option<u64> = None;
    let mut slowest_name: Option<String> = None;
    for entry in events.iter()? {
        let (_, v) = entry?;
        let event = decode_event_row(v.value())?;
        if event.session_id != Some(session_id) {
            continue;
        }
        match event.kind {
            EventKind::CompileStart => start_count += 1,
            EventKind::CompileEnd => end_count += 1,
            EventKind::SessionStart | EventKind::SessionEnd => {}
        }
        if let Some(d) = event.duration_us {
            if d > slowest_us.unwrap_or(0) {
                slowest_us = Some(d);
                slowest_name = event.crate_name.clone();
            }
        }
    }
    let count = if end_count > 0 {
        end_count
    } else {
        start_count
    };
    Ok((count, slowest_us, slowest_name))
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
            let event = decode_event_row(v.value())?;
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
        assert_eq!(count, 1);
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
                    cache_summary: None,
                    log_paths: None,
                    miss_reasons: Vec::new(),
                },
            )
            .expect("upsert");
        }
        let slow = list_slow_builds(&path, 1000, 10).expect("slow");
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
                    cache_summary: None,
                    log_paths: None,
                    miss_reasons: Vec::new(),
                },
            )
            .expect("upsert");
        }
        let list = list_builds(&path, 10, None).expect("list");
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].session_id, 30);
        assert_eq!(list[1].session_id, 20);
        assert_eq!(list[2].session_id, 10);
        let filtered = list_builds(&path, 10, Some(base + 150)).expect("filtered");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session_id, 30);
        let limited = list_builds(&path, 2, None).expect("limited");
        assert_eq!(limited.len(), 2);
    }

    crate::timed_test!(finalize_build_preserves_existing_fields_in_one_txn, {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("state.redb");
        upsert_build(
            &path,
            &BuildRecord {
                session_id: 5,
                repo_root: "/repo".into(),
                started_at_ms: 1_000,
                ended_at_ms: None,
                exit_code: None,
                total_wall_ms: None,
                crate_count: 0,
                slowest_crate_us: None,
                slowest_crate_name: None,
                cache_summary: None,
                log_paths: None,
                miss_reasons: Vec::new(),
            },
        )
        .expect("seed record");
        let record = finalize_build(&path, 5, 0, 3_000, (4, Some(9_000), Some("slow".into())))
            .expect("finalize");
        assert_eq!(record.repo_root, "/repo");
        assert_eq!(record.started_at_ms, 1_000);
        assert_eq!(record.total_wall_ms, Some(2_000));
        assert_eq!(record.crate_count, 4);
        let stored = get_build(&path, 5).expect("get").expect("record");
        assert_eq!(stored, record);

        // Unknown session: a default record is minted in the same txn.
        let minted = finalize_build(&path, 6, 1, 4_000, (0, None, None)).expect("finalize unknown");
        assert_eq!(minted.started_at_ms, 4_000);
        assert_eq!(minted.exit_code, Some(1));
        assert_eq!(minted.total_wall_ms, Some(0));
    });

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

    /// `ensure_initialized` evicts pre-#580 (untagged) rows on first
    /// startup. Subsequent calls find nothing to drop.
    #[test]
    fn migration_drops_pre_580_untagged_rows() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("state.redb");
        // Seed the BUILDS table with a raw-bytes row that doesn't carry
        // the 0x01 tag (simulating a pre-#580 bincode-encoded row).
        {
            let db = open_db(&path).expect("open");
            init_tables(&db).expect("init");
            let txn = db.begin_write().expect("begin");
            {
                let mut builds = txn.open_table(BUILDS).expect("builds");
                builds
                    .insert(42u64, &[0x00, 0x42, 0xDE, 0xAD][..])
                    .expect("insert untagged");
            }
            txn.commit().expect("commit");
        }
        // Migration removes the untagged row.
        ensure_initialized(&path).expect("ensure");
        // Verify the row is gone via list_builds (now empty).
        let list = list_builds(&path, 10, None).expect("list");
        assert!(list.is_empty(), "untagged pre-#580 row must be dropped");
    }
}
