//! Daemon-side state tables for build session correlation, stored in the
//! shared `~/.soldr/state.sqlite3`.
//!
//! ## Open once, not once per operation (soldr#2224)
//!
//! Every public entry point comes in two shapes:
//!
//! * `foo(db_path, ..)` — opens the database, runs one operation, drops
//!   the handle. Convenient, and correct for a caller that does exactly
//!   one thing.
//! * [`foo_in(&db, ..)`](get_build_in) — runs against a handle the caller
//!   already owns.
//!
//! A caller performing several operations back to back should still open
//! one [`StateDbHandle`] via [`open_handle`] and use the `_in` variants —
//! not for lock hygiene anymore (SQLite WAL replaced redb's exclusive
//! whole-file lock, the cause of the soldr#2223 lock storm), but because
//! one connection open per logical operation is simply cheaper.
//!
//! Tables live alongside the existing `target_registry_targets`:
//! - `daemon_builds`        : u64 session_id → tagged-byte BuildRecord
//! - `daemon_events`        : u64 event_id   → tagged-byte Event
//! - `daemon_meta`          : `&str` key     → u64 (next event id, ...)
//!
//! `u64` ids are stored bit-cast as SQLite `INTEGER` (i64); only equality
//! is used on them, which the bit-cast preserves.
//!
//! ## Serialization (issue #603)
//!
//! Every value in every table is `[0x01][prost body]` — no other shape is
//! accepted. The encoding survived the redb→SQLite engine swap verbatim;
//! the pre-#580 bincode-row sweep is gone because a legacy redb file is
//! deleted whole by `state_store` on first SQLite open.

use crate::cache_lib::state_store::{open_state_db, StateDbHandle};
use crate::cache_lib::target_registry::RegistryError;
use crate::daemon::protocol::{BuildRecord, WireDecodeError};
use crate::daemon::wire::{self, prost_tagged_bytes, REDB_TAG_PROST};
use prost::Message;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

fn wire_err(e: WireDecodeError) -> RegistryError {
    RegistryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn prost_decode_err(e: prost::DecodeError) -> RegistryError {
    RegistryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

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

fn open_db(path: &Path) -> Result<StateDbHandle, RegistryError> {
    Ok(open_state_db(path)?)
}

/// Public [`open_db`]: acquire the state database **once** for a caller
/// that is about to run several operations through the `_in` variants
/// (soldr#2224).
pub fn open_handle(db_path: &Path) -> Result<StateDbHandle, RegistryError> {
    open_db(db_path)
}

/// Open + initialize the daemon tables in the shared state store.
/// Idempotent — the schema is created by the open itself.
pub fn ensure_initialized(db_path: &Path) -> Result<(), RegistryError> {
    let _db = open_db(db_path)?;
    Ok(())
}

fn next_event_id(db: &Connection) -> Result<u64, RegistryError> {
    let tx = db.unchecked_transaction()?;
    let current: i64 = tx
        .query_row(
            "SELECT value FROM daemon_meta WHERE key = ?1",
            params![META_NEXT_EVENT_ID],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(1);
    let next = (current as u64).saturating_add(1);
    tx.execute(
        "INSERT INTO daemon_meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![META_NEXT_EVENT_ID, next as i64],
    )?;
    tx.commit()?;
    Ok(current as u64)
}

/// Append one event using a handle the caller already owns (soldr#2224).
pub fn append_event_in(db: &Connection, event: &Event) -> Result<(), RegistryError> {
    let id = next_event_id(db)?;
    let bytes = prost_tagged_bytes(&wire::event_to_wire(event));
    db.execute(
        "INSERT INTO daemon_events(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![id as i64, bytes],
    )?;
    Ok(())
}

pub fn append_event(db_path: &Path, event: &Event) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    append_event_in(&db, event)
}

/// Insert-or-replace a build record using a caller-owned handle (soldr#2224).
pub fn upsert_build_in(db: &Connection, record: &BuildRecord) -> Result<(), RegistryError> {
    let bytes = prost_tagged_bytes(&wire::build_record_to_wire(record));
    db.execute(
        "INSERT INTO daemon_builds(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![record.session_id as i64, bytes],
    )?;
    Ok(())
}

pub fn upsert_build(db_path: &Path, record: &BuildRecord) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    upsert_build_in(&db, record)
}

/// Read one build record using a caller-owned handle (soldr#2224).
pub fn get_build_in(
    db: &Connection,
    session_id: u64,
) -> Result<Option<BuildRecord>, RegistryError> {
    let row: Option<Vec<u8>> = db
        .query_row(
            "SELECT value FROM daemon_builds WHERE key = ?1",
            params![session_id as i64],
            |row| row.get(0),
        )
        .optional()?;
    match row {
        Some(bytes) => decode_build_row(&bytes).map(Some),
        None => Ok(None),
    }
}

pub fn get_build(db_path: &Path, session_id: u64) -> Result<Option<BuildRecord>, RegistryError> {
    let db = open_db(db_path)?;
    get_build_in(&db, session_id)
}

/// Every `(session_id, value)` build row, raw.
fn raw_build_rows(db: &Connection) -> Result<Vec<(i64, Vec<u8>)>, RegistryError> {
    let mut statement = db.prepare("SELECT key, value FROM daemon_builds")?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get::<_, Vec<u8>>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Every `(event_id, value)` event row, raw.
fn raw_event_rows(db: &Connection) -> Result<Vec<(i64, Vec<u8>)>, RegistryError> {
    let mut statement = db.prepare("SELECT key, value FROM daemon_events")?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get::<_, Vec<u8>>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
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
    let db = open_db(db_path)?;
    let tx = db.unchecked_transaction()?;
    let mut updated = 0_u64;
    for id in session_ids
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>()
    {
        let row: Option<Vec<u8>> = tx
            .query_row(
                "SELECT value FROM daemon_builds WHERE key = ?1",
                params![id as i64],
                |row| row.get(0),
            )
            .optional()?;
        let Some(bytes) = row else {
            continue;
        };
        let mut record = decode_build_row(&bytes)?;
        let Some(paths) = record.log_paths.as_mut() else {
            continue;
        };
        paths.archived_session_log_path = None;
        paths.archived_journal_path = None;
        paths.archived_session_stats_path = None;
        paths.archived_compile_journal_path = None;
        let bytes = prost_tagged_bytes(&wire::build_record_to_wire(&record));
        tx.execute(
            "UPDATE daemon_builds SET value = ?2 WHERE key = ?1",
            params![id as i64, bytes],
        )?;
        updated += 1;
    }
    tx.commit()?;
    Ok(updated)
}

/// Clear only the stale fixed-name zccache session artifacts from completed
/// history rows. Build-scoped stats and compile-journal archives remain
/// available (#1827).
pub fn clear_legacy_archive_paths(
    db_path: &Path,
    session_ids: &[u64],
) -> Result<u64, RegistryError> {
    if session_ids.is_empty() {
        return Ok(0);
    }
    let db = open_db(db_path)?;
    let tx = db.unchecked_transaction()?;
    let mut updated = 0_u64;
    for id in session_ids
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>()
    {
        let row: Option<Vec<u8>> = tx
            .query_row(
                "SELECT value FROM daemon_builds WHERE key = ?1",
                params![id as i64],
                |row| row.get(0),
            )
            .optional()?;
        let Some(bytes) = row else {
            continue;
        };
        let mut record = decode_build_row(&bytes)?;
        let Some(paths) = record.log_paths.as_mut() else {
            continue;
        };
        if paths.archived_session_log_path.is_none()
            && paths.archived_journal_path.is_none()
            && paths.session_log_path.is_none()
            && paths.journal_path.is_none()
        {
            continue;
        }
        paths.session_log_path = None;
        paths.journal_path = None;
        paths.archived_session_log_path = None;
        paths.archived_journal_path = None;
        let bytes = prost_tagged_bytes(&wire::build_record_to_wire(&record));
        tx.execute(
            "UPDATE daemon_builds SET value = ?2 WHERE key = ?1",
            params![id as i64, bytes],
        )?;
        updated += 1;
    }
    tx.commit()?;
    Ok(updated)
}

/// Read-modify-write finalization of a BuildRecord in ONE open + one
/// transaction (soldr#1536).
pub fn finalize_build(
    db_path: &Path,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
    aggregate: (u32, Option<u64>, Option<String>),
) -> Result<BuildRecord, RegistryError> {
    let (crate_count, slowest_crate_us, slowest_crate_name) = aggregate;
    let db = open_db(db_path)?;
    let tx = db.unchecked_transaction()?;
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT value FROM daemon_builds WHERE key = ?1",
            params![session_id as i64],
            |row| row.get(0),
        )
        .optional()?;
    let mut record = match existing {
        Some(bytes) => decode_build_row(&bytes)?,
        None => BuildRecord {
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
        },
    };
    record.ended_at_ms = Some(ended_at_ms);
    record.exit_code = Some(exit_code);
    record.total_wall_ms = Some((ended_at_ms - record.started_at_ms).max(0) as u64);
    record.crate_count = crate_count;
    record.slowest_crate_us = slowest_crate_us;
    record.slowest_crate_name = slowest_crate_name;
    let bytes = prost_tagged_bytes(&wire::build_record_to_wire(&record));
    tx.execute(
        "INSERT INTO daemon_builds(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![session_id as i64, bytes],
    )?;
    tx.commit()?;
    Ok(record)
}

/// Strict prost-only decoder. A row that doesn't carry the prost tag
/// byte surfaces an `InvalidData` error — the whole-file legacy cleanup
/// guarantees no such rows exist, so this branch only fires if the file
/// is corrupted in flight.
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
            "state row missing prost tag byte (post-#603 daemon expects every value to start with 0x01)",
        ))),
    }
}

pub fn list_builds(
    db_path: &Path,
    limit: u32,
    since_ms: Option<i64>,
) -> Result<Vec<BuildRecord>, RegistryError> {
    let db = open_db(db_path)?;
    let mut rows: Vec<BuildRecord> = Vec::new();
    for (_, bytes) in raw_build_rows(&db)? {
        let record = decode_build_row(&bytes)?;
        if let Some(cutoff) = since_ms {
            if record.started_at_ms < cutoff {
                continue;
            }
        }
        rows.push(record);
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.started_at_ms));
    rows.truncate(limit as usize);
    Ok(rows)
}

pub fn list_slow_builds(
    db_path: &Path,
    threshold_ms: u64,
    limit: u32,
) -> Result<Vec<BuildRecord>, RegistryError> {
    let db = open_db(db_path)?;
    let mut rows: Vec<BuildRecord> = Vec::new();
    for (_, bytes) in raw_build_rows(&db)? {
        let record = decode_build_row(&bytes)?;
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
    list_events_for_session_in(&db, session_id)
}

/// [`list_events_for_session`] against a caller-owned handle (soldr#2224).
pub fn list_events_for_session_in(
    db: &Connection,
    session_id: u64,
) -> Result<Vec<Event>, RegistryError> {
    let mut rows: Vec<Event> = Vec::new();
    for (_, bytes) in raw_event_rows(db)? {
        let event = decode_event_row(&bytes)?;
        if event.session_id == Some(session_id) {
            rows.push(event);
        }
    }
    rows.sort_by_key(|row| row.ts_ms);
    Ok(rows)
}

/// Walk events for `session_id`, returning `(crate_count, slowest_us, slowest_name)`.
pub fn aggregate_session(
    db_path: &Path,
    session_id: u64,
) -> Result<(u32, Option<u64>, Option<String>), RegistryError> {
    let db = open_db(db_path)?;
    aggregate_session_in(&db, session_id)
}

/// [`aggregate_session`] against a caller-owned handle (soldr#2224).
pub fn aggregate_session_in(
    db: &Connection,
    session_id: u64,
) -> Result<(u32, Option<u64>, Option<String>), RegistryError> {
    let mut start_count: u32 = 0;
    let mut end_count: u32 = 0;
    let mut slowest_us: Option<u64> = None;
    let mut slowest_name: Option<String> = None;
    for (_, bytes) in raw_event_rows(db)? {
        let event = decode_event_row(&bytes)?;
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
    let mut to_delete: Vec<i64> = Vec::new();
    for (id, bytes) in raw_event_rows(&db)? {
        let event = decode_event_row(&bytes)?;
        if event.ts_ms < cutoff_ms {
            to_delete.push(id);
        }
    }
    if to_delete.is_empty() {
        return Ok(0);
    }
    let tx = db.unchecked_transaction()?;
    let mut removed: u64 = 0;
    for id in &to_delete {
        removed += tx.execute("DELETE FROM daemon_events WHERE key = ?1", params![id])? as u64;
    }
    tx.commit()?;
    Ok(removed)
}

/// Convenience wrapper used by integration tests and the CLI: derive
/// the db path from `SoldrPaths` once.
pub fn db_path(paths: &crate::core::SoldrPaths) -> PathBuf {
    crate::cache_lib::data_db_path(paths)
}
