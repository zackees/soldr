//! Daemon-side redb tables for build session correlation and the
//! linked-zccache runtime identity. Opens `~/.soldr/state.redb` per call and drops
//! the handle on return — redb refuses concurrent multi-process opens
//! and the wrapper / GC tools open the same file directly.
//!
//! Tables live alongside the existing `target_registry_targets`:
//! - `daemon_builds`  : u64 session_id → tagged-byte BuildRecord
//! - `daemon_events`  : u64 event_id   → tagged-byte Event
//! - `daemon_meta`    : `&str` key     → u64 (next event id, ...)
//! - `daemon_zccache_link` : `&str` key → tagged-byte ZccacheDaemonLink
//!
//! ## Serialization migration (issue #580)
//!
//! Every value written by this module is now `[0x01 tag][prost body]`
//! (see [`crate::daemon::wire::REDB_TAG_PROST`] +
//! [`crate::daemon::wire::prost_tagged_bytes`]). Reads classify the
//! first byte:
//!
//! * `0x01` → strip the tag and prost-decode (the new format).
//! * Anything else → bincode-deserialize the full slice (the legacy
//!   format that pre-#580 daemons wrote into the same tables).
//!
//! The `LegacyZccacheDaemonLink` shim from #265 remains in place as a
//! second-stage fallback under the bincode branch — it covers the
//! even-older "before-private-daemon" shape that lacks fields the
//! current `ZccacheDaemonLink` adds. So a redb row from ANY soldr
//! version this codebase has ever shipped still decodes.

use crate::cache_lib::target_registry::RegistryError;
use crate::daemon::protocol::{BuildRecord, WireDecodeError, ZccacheDaemonLink};
use crate::daemon::wire::{self, classify_redb_row, prost_tagged_bytes, RedbDecode};
use prost::Message;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
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
const ZCCACHE_LINK: TableDefinition<&str, &[u8]> = TableDefinition::new("daemon_zccache_link");

const META_NEXT_EVENT_ID: &str = "next_event_id";
const LINKED_ZCCACHE_KEY: &str = "active";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LegacyZccacheDaemonLink {
    binary_path: String,
    cache_dir: String,
    session_id: Option<String>,
    source: String,
}

impl From<LegacyZccacheDaemonLink> for ZccacheDaemonLink {
    fn from(value: LegacyZccacheDaemonLink) -> Self {
        Self {
            binary_path: value.binary_path,
            cache_dir: value.cache_dir,
            session_id: value.session_id,
            source: value.source,
            private_daemon: false,
            daemon_name: None,
            owner_pid: None,
            private_env_keys: Vec::new(),
        }
    }
}

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
        let _ = txn.open_table(ZCCACHE_LINK)?;
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
    Ok(Some(deserialize_build_record(row.value())?))
}

/// Read a `BuildRecord` from a redb row, dispatching on the leading
/// tag byte. New rows (0x01) decode via prost; legacy bytes fall
/// through to bincode.
fn deserialize_build_record(bytes: &[u8]) -> Result<BuildRecord, RegistryError> {
    match classify_redb_row(bytes) {
        RedbDecode::Prost(rest) => {
            let wire = wire::proto::WireBuildRecord::decode(rest).map_err(prost_decode_err)?;
            Ok(wire::build_record_from_wire(wire))
        }
        RedbDecode::LegacyBincode(bytes) => {
            bincode::deserialize::<BuildRecord>(bytes).map_err(bincode_err)
        }
    }
}

/// Read an `Event` from a redb row, with the same tag-byte dispatch.
fn deserialize_event(bytes: &[u8]) -> Result<Event, RegistryError> {
    match classify_redb_row(bytes) {
        RedbDecode::Prost(rest) => {
            let wire = wire::proto::WireEvent::decode(rest).map_err(prost_decode_err)?;
            wire::event_from_wire(wire).map_err(wire_err)
        }
        RedbDecode::LegacyBincode(bytes) => {
            bincode::deserialize::<Event>(bytes).map_err(bincode_err)
        }
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
        let record: BuildRecord = deserialize_build_record(v.value())?;
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
        let record: BuildRecord = deserialize_build_record(v.value())?;
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
        let event: Event = deserialize_event(v.value())?;
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

/// Set the linked zccache runtime/cache/session identity. None clears it.
pub fn set_linked_zccache(
    db_path: &Path,
    link: Option<&ZccacheDaemonLink>,
) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let txn = db.begin_write()?;
    {
        let mut table = txn.open_table(ZCCACHE_LINK)?;
        match link {
            Some(link) => {
                let bytes = prost_tagged_bytes(&wire::zccache_link_to_wire(link));
                table.insert(LINKED_ZCCACHE_KEY, bytes.as_slice())?;
            }
            None => {
                table.remove(LINKED_ZCCACHE_KEY)?;
            }
        }
    }
    txn.commit()?;
    Ok(())
}

pub fn get_linked_zccache(db_path: &Path) -> Result<Option<ZccacheDaemonLink>, RegistryError> {
    let db = open_db(db_path)?;
    init_tables(&db)?;
    let txn = db.begin_read()?;
    let table = txn.open_table(ZCCACHE_LINK)?;
    let Some(row) = table.get(LINKED_ZCCACHE_KEY)? else {
        return Ok(None);
    };
    let link = deserialize_zccache_daemon_link(row.value())?;
    Ok(Some(link))
}

/// Three-stage fallback decoder:
///
/// 1. `0x01` tag prefix → prost-decode (the #580 format new daemons write).
/// 2. Bincode current `ZccacheDaemonLink` shape (pre-#580 daemons).
/// 3. Bincode legacy [`LegacyZccacheDaemonLink`] shape lifted via `From`
///    (pre-#265 even-older daemons that lacked private-daemon fields).
fn deserialize_zccache_daemon_link(bytes: &[u8]) -> Result<ZccacheDaemonLink, RegistryError> {
    match classify_redb_row(bytes) {
        RedbDecode::Prost(rest) => {
            let wire =
                wire::proto::WireZccacheDaemonLink::decode(rest).map_err(prost_decode_err)?;
            Ok(wire::zccache_link_from_wire(wire))
        }
        RedbDecode::LegacyBincode(bytes) => {
            match bincode::deserialize::<ZccacheDaemonLink>(bytes) {
                Ok(link) => Ok(link),
                Err(new_err) => match bincode::deserialize::<LegacyZccacheDaemonLink>(bytes) {
                    Ok(legacy) => Ok(legacy.into()),
                    Err(_) => Err(bincode_err(new_err)),
                },
            }
        }
    }
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
            let event = deserialize_event(v.value())?;
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
    fn linked_zccache_identity_round_trips() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("state.redb");
        let link = ZccacheDaemonLink {
            binary_path: "/tmp/zccache".into(),
            cache_dir: "/tmp/cache".into(),
            session_id: Some("session-1".into()),
            source: "managed".into(),
            private_daemon: true,
            daemon_name: Some("soldr-dev-test".into()),
            owner_pid: Some(1234),
            private_env_keys: vec!["ZCCACHE_PATH_REMAP".into()],
        };
        assert_eq!(get_linked_zccache(&path).expect("get"), None);
        set_linked_zccache(&path, Some(&link)).expect("set");
        assert_eq!(get_linked_zccache(&path).expect("get"), Some(link));
        set_linked_zccache(&path, None).expect("clear");
        assert_eq!(get_linked_zccache(&path).expect("get"), None);
    }

    #[test]
    fn linked_zccache_legacy_identity_decodes_as_shared_daemon() {
        let legacy = LegacyZccacheDaemonLink {
            binary_path: "/tmp/zccache".into(),
            cache_dir: "/tmp/cache".into(),
            session_id: Some("session-1".into()),
            source: "managed".into(),
        };
        let bytes = bincode::serialize(&legacy).expect("serialize legacy");
        let decoded = deserialize_zccache_daemon_link(&bytes).expect("decode legacy");
        assert_eq!(decoded.binary_path, legacy.binary_path);
        assert_eq!(decoded.cache_dir, legacy.cache_dir);
        assert_eq!(decoded.session_id, legacy.session_id);
        assert_eq!(decoded.source, legacy.source);
        assert!(!decoded.private_daemon);
        assert_eq!(decoded.daemon_name, None);
        assert_eq!(decoded.owner_pid, None);
        assert!(decoded.private_env_keys.is_empty());
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
