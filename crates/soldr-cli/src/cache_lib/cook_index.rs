//! Cross-repo shared `soldr cook` artifact index (issue #576, meta
//! #579). This module owns a redb table inside the shared
//! `~/.soldr/state.redb` and gives the daemon a per-request-open
//! surface to look up, record, touch, and stat cook artifacts. Heavy
//! I/O (tar+zstd:19 packing) lives in the PR 2 `soldr cook` worker;
//! this module never reads or writes the artifact bytes themselves —
//! it only tracks where they are and how big they are.
//!
//! Key tuple (prost-encoded blob, used as the redb key):
//!
//! ```text
//! (recipe_hash: [u8; 32], target_triple, profile, channel, rustc_version)
//! ```
//!
//! Cross-target sharing is structurally impossible by including
//! triple/profile/channel/rustc in the key — there is no code path
//! that can relax this without changing the schema.
//!
//! ## Storage (post-#603)
//!
//! `cook_index_v2` is the only live table. Keys are bare prost-encoded
//! `WireCookKey` bytes; values are `[0x01][prost-encoded WireCookEntry]`
//! tagged-byte rows.
//!
//! The pre-#603 `cook_index_v1` table (bincode) is **dropped on first
//! call to [`ensure_initialized`]** post-upgrade. v1 entries become
//! orphaned; their on-disk cook artifacts under
//! `~/.soldr/cache/cook/` remain and will be re-indexed by the next
//! `soldr cook` invocation.
//!
//! A future schema change MUST land as `cook_index_v3` rather than
//! mutating v2 in place.

use crate::cache_lib::redb_lock::{state_db_open_lock, StateDbHandle};
use crate::cache_lib::target_registry::RegistryError;
use crate::daemon::protocol::WireDecodeError;
use crate::daemon::wire::{prost_tagged_bytes, proto, REDB_TAG_PROST};
use prost::Message;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;

const COOK_INDEX_V1: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cook_index_v1");
const COOK_INDEX_V2: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cook_index_v2");

/// Lookup key for the cook artifact index. Cross-target sharing is
/// forbidden by including triple/profile/channel/rustc in the key —
/// two builds that differ on any of these resolve to different rows
/// regardless of recipe_hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CookKey {
    pub recipe_hash: [u8; 32],
    pub target_triple: String,
    pub profile: String,
    pub channel: String,
    pub rustc_version: String,
}

/// Stored value for a cook artifact entry. PR 2 writes these via
/// `CookRecord`; PR 3 reads them via `CookLookup` + `CookTouch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookEntry {
    pub sha256: [u8; 32],
    pub size_bytes: u64,
    pub created_unix_ms: u64,
    pub last_used_unix_ms: u64,
    /// Best-effort, normalized git remote URL. None when the source
    /// workspace had no `.git/` at cook time or `git config
    /// remote.origin.url` was unset.
    pub origin_url_normalized: Option<String>,
    /// Human-readable cook command summary stored for diagnostics
    /// (e.g. shown by `soldr cache stats`).
    pub cook_cmd_summary: String,
}

fn prost_decode_err(e: prost::DecodeError) -> RegistryError {
    RegistryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn wire_err(e: WireDecodeError) -> RegistryError {
    RegistryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Acquire the shared [`state_db_open_lock`] and open the redb file
/// under it. See `cache_lib::redb_lock` for why the handle holds the
/// lock for the entire lifetime of the [`Database`]: redb refuses
/// concurrent in-process opens of the same file and would otherwise
/// race with [`crate::daemon::db`] (issue #608).
fn open_db(path: &Path) -> Result<StateDbHandle, RegistryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let guard = state_db_open_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let db = Database::builder().create(path)?;
    Ok(StateDbHandle::new(db, guard))
}

fn init_v2(db: &Database) -> Result<(), RegistryError> {
    let txn = db.begin_write()?;
    {
        let _ = txn.open_table(COOK_INDEX_V2)?;
    }
    txn.commit()?;
    Ok(())
}

/// Drop the legacy `cook_index_v1` table entirely. Idempotent — on a
/// fresh checkout (or a daemon that has already run this once) there
/// is nothing to drop. On a daemon upgrading from pre-#603 the v1
/// table is removed and its rows are gone. The on-disk cook artifacts
/// under `~/.soldr/cache/cook/` are unaffected; they get re-indexed on
/// the next `soldr cook`.
fn migrate_drop_v1(db: &Database) -> Result<(), RegistryError> {
    let mut v1_rows: u64 = 0;
    {
        let txn = db.begin_read()?;
        if let Ok(table) = txn.open_table(COOK_INDEX_V1) {
            for entry in table.iter()? {
                let _ = entry?;
                v1_rows += 1;
            }
        }
    }
    if v1_rows == 0 {
        // Drop the empty table if it exists; ignore "doesn't exist".
        let txn = db.begin_write()?;
        let _ = txn.delete_table(COOK_INDEX_V1);
        txn.commit()?;
        return Ok(());
    }
    let txn = db.begin_write()?;
    let _ = txn.delete_table(COOK_INDEX_V1);
    txn.commit()?;
    eprintln!(
        "soldr-daemon: dropped {v1_rows} pre-#580 cook_index_v1 row(s) during one-time format \
         migration; cook artifacts on disk are unaffected and will be re-indexed on the next \
         `soldr cook` invocation",
    );
    Ok(())
}

/// Idempotent: open the `state.redb` file, ensure `cook_index_v2`
/// exists, and drop the legacy `cook_index_v1` table if any rows
/// remain from a pre-#580 daemon.
pub fn ensure_initialized(db_path: &Path) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    init_v2(&db)?;
    migrate_drop_v1(&db)?;
    Ok(())
}

// =========================================================================
// Encoding helpers
// =========================================================================

fn cook_key_to_wire(key: &CookKey) -> proto::WireCookKey {
    proto::WireCookKey {
        recipe_hash: key.recipe_hash.to_vec(),
        target_triple: key.target_triple.clone(),
        profile: key.profile.clone(),
        channel: key.channel.clone(),
        rustc_version: key.rustc_version.clone(),
    }
}

fn cook_key_from_wire(wire: proto::WireCookKey) -> Result<CookKey, WireDecodeError> {
    if wire.recipe_hash.len() != 32 {
        return Err(WireDecodeError::InvalidShaLength(wire.recipe_hash.len()));
    }
    let mut recipe_hash = [0u8; 32];
    recipe_hash.copy_from_slice(&wire.recipe_hash);
    Ok(CookKey {
        recipe_hash,
        target_triple: wire.target_triple,
        profile: wire.profile,
        channel: wire.channel,
        rustc_version: wire.rustc_version,
    })
}

fn cook_entry_to_wire(entry: &CookEntry) -> proto::WireCookEntry {
    proto::WireCookEntry {
        sha256: entry.sha256.to_vec(),
        size_bytes: entry.size_bytes,
        created_unix_ms: entry.created_unix_ms,
        last_used_unix_ms: entry.last_used_unix_ms,
        origin_url_normalized: entry.origin_url_normalized.clone(),
        cook_cmd_summary: entry.cook_cmd_summary.clone(),
    }
}

fn cook_entry_from_wire(wire: proto::WireCookEntry) -> Result<CookEntry, WireDecodeError> {
    if wire.sha256.len() != 32 {
        return Err(WireDecodeError::InvalidShaLength(wire.sha256.len()));
    }
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(&wire.sha256);
    Ok(CookEntry {
        sha256,
        size_bytes: wire.size_bytes,
        created_unix_ms: wire.created_unix_ms,
        last_used_unix_ms: wire.last_used_unix_ms,
        origin_url_normalized: wire.origin_url_normalized,
        cook_cmd_summary: wire.cook_cmd_summary,
    })
}

fn encode_key(key: &CookKey) -> Vec<u8> {
    let wire = cook_key_to_wire(key);
    let mut out = Vec::with_capacity(wire.encoded_len());
    wire.encode(&mut out).expect("Vec write is infallible");
    out
}

fn decode_key(bytes: &[u8]) -> Result<CookKey, RegistryError> {
    let wire = proto::WireCookKey::decode(bytes).map_err(prost_decode_err)?;
    cook_key_from_wire(wire).map_err(wire_err)
}

/// Strict prost-only decode of a `cook_index_v2` value. Values that
/// don't carry the `0x01` tag (impossible under v2 by construction)
/// surface an `InvalidData` error.
fn decode_entry(bytes: &[u8]) -> Result<CookEntry, RegistryError> {
    let rest = match bytes.split_first() {
        Some((&REDB_TAG_PROST, rest)) => rest,
        _ => {
            return Err(RegistryError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cook_index_v2 row missing prost tag byte",
            )))
        }
    };
    let wire = proto::WireCookEntry::decode(rest).map_err(prost_decode_err)?;
    cook_entry_from_wire(wire).map_err(wire_err)
}

// =========================================================================
// Public surface
// =========================================================================

/// Upsert a cook artifact entry.
pub fn upsert(db_path: &Path, key: &CookKey, entry: &CookEntry) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    init_v2(&db)?;
    let key_bytes = encode_key(key);
    let value_bytes = prost_tagged_bytes(&cook_entry_to_wire(entry));
    let txn = db.begin_write()?;
    {
        let mut table = txn.open_table(COOK_INDEX_V2)?;
        table.insert(key_bytes.as_slice(), value_bytes.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// Look up a single entry by the full key tuple.
pub fn lookup(db_path: &Path, key: &CookKey) -> Result<Option<CookEntry>, RegistryError> {
    let db = open_db(db_path)?;
    init_v2(&db)?;
    let key_bytes = encode_key(key);
    let txn = db.begin_read()?;
    let table = txn.open_table(COOK_INDEX_V2)?;
    let Some(row) = table.get(key_bytes.as_slice())? else {
        return Ok(None);
    };
    Ok(Some(decode_entry(row.value())?))
}

/// Drift diagnostic: given a (missing) lookup key, scan v2 for other
/// rows whose `(origin_url_normalized, target_triple, profile,
/// channel, rustc_version)` match — but whose `recipe_hash` differs.
/// Returns up to `limit` previous recipe hashes, newest-first.
pub fn drift_recipe_hashes(
    db_path: &Path,
    miss_key: &CookKey,
    origin_url_normalized: Option<&str>,
    limit: usize,
) -> Result<Vec<[u8; 32]>, RegistryError> {
    let Some(origin) = origin_url_normalized else {
        return Ok(Vec::new());
    };
    let db = open_db(db_path)?;
    init_v2(&db)?;
    let txn = db.begin_read()?;
    let table = txn.open_table(COOK_INDEX_V2)?;
    let mut out: Vec<([u8; 32], u64)> = Vec::new();
    for entry in table.iter()? {
        let (k_bytes, v_bytes) = entry?;
        let Ok(stored_key) = decode_key(k_bytes.value()) else {
            continue;
        };
        if !key_matches_origin_matrix(&stored_key, miss_key) {
            continue;
        }
        let Ok(stored_entry) = decode_entry(v_bytes.value()) else {
            continue;
        };
        if stored_entry.origin_url_normalized.as_deref() != Some(origin) {
            continue;
        }
        out.push((stored_key.recipe_hash, stored_entry.last_used_unix_ms));
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out.truncate(limit);
    Ok(out.into_iter().map(|(h, _)| h).collect())
}

fn key_matches_origin_matrix(stored: &CookKey, miss: &CookKey) -> bool {
    if stored.recipe_hash == miss.recipe_hash {
        return false;
    }
    stored.target_triple == miss.target_triple
        && stored.profile == miss.profile
        && stored.channel == miss.channel
        && stored.rustc_version == miss.rustc_version
}

/// Bump the `last_used_unix_ms` field for the entry whose `sha256`
/// matches.
pub fn touch(db_path: &Path, sha256: &[u8; 32], now_unix_ms: u64) -> Result<bool, RegistryError> {
    let db = open_db(db_path)?;
    init_v2(&db)?;
    let mut updates: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    {
        let txn = db.begin_read()?;
        let table = txn.open_table(COOK_INDEX_V2)?;
        for entry in table.iter()? {
            let (k_bytes, v_bytes) = entry?;
            let Ok(mut value) = decode_entry(v_bytes.value()) else {
                continue;
            };
            if &value.sha256 == sha256 {
                value.last_used_unix_ms = now_unix_ms;
                let new_v = prost_tagged_bytes(&cook_entry_to_wire(&value));
                updates.push((k_bytes.value().to_vec(), new_v));
            }
        }
    }
    if updates.is_empty() {
        return Ok(false);
    }
    let txn = db.begin_write()?;
    {
        let mut table = txn.open_table(COOK_INDEX_V2)?;
        for (k, v) in &updates {
            table.insert(k.as_slice(), v.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(true)
}

/// Collect every `(CookKey, CookEntry)` row in `cook_index_v2` for
/// ranking / filtering by an eviction pass. Undecodable rows are
/// silently skipped — eviction is best-effort and we never want a
/// single corrupt row to wedge the pass.
pub fn iter_entries(db_path: &Path) -> Result<Vec<(CookKey, CookEntry)>, RegistryError> {
    let db = open_db(db_path)?;
    init_v2(&db)?;
    let txn = db.begin_read()?;
    let table = txn.open_table(COOK_INDEX_V2)?;
    let mut out: Vec<(CookKey, CookEntry)> = Vec::new();
    for entry in table.iter()? {
        let (k_bytes, v_bytes) = entry?;
        let Ok(key) = decode_key(k_bytes.value()) else {
            continue;
        };
        let Ok(value) = decode_entry(v_bytes.value()) else {
            continue;
        };
        out.push((key, value));
    }
    Ok(out)
}

/// Delete every `cook_index_v2` row whose decoded entry carries the
/// supplied `sha256`. Returns `true` when at least one row was
/// removed. Does NOT touch the on-disk `<sha256>.tar.zst` artifact —
/// the eviction pass is responsible for unlinking that file
/// separately.
pub fn evict(db_path: &Path, sha256: &[u8; 32]) -> Result<bool, RegistryError> {
    let db = open_db(db_path)?;
    init_v2(&db)?;
    let mut victims: Vec<Vec<u8>> = Vec::new();
    {
        let txn = db.begin_read()?;
        let table = txn.open_table(COOK_INDEX_V2)?;
        for entry in table.iter()? {
            let (k_bytes, v_bytes) = entry?;
            let Ok(value) = decode_entry(v_bytes.value()) else {
                continue;
            };
            if &value.sha256 == sha256 {
                victims.push(k_bytes.value().to_vec());
            }
        }
    }
    if victims.is_empty() {
        return Ok(false);
    }
    let txn = db.begin_write()?;
    {
        let mut table = txn.open_table(COOK_INDEX_V2)?;
        for k in &victims {
            table.remove(k.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(true)
}

/// Aggregate statistics surfaced via `Status` and `soldr daemon
/// status`: returns `(entry_count, total_size_bytes)`.
pub fn stats(db_path: &Path) -> Result<(u64, u64), RegistryError> {
    let db = open_db(db_path)?;
    init_v2(&db)?;
    let txn = db.begin_read()?;
    let table = txn.open_table(COOK_INDEX_V2)?;
    let mut count: u64 = 0;
    let mut total: u64 = 0;
    for entry in table.iter()? {
        let (_, v_bytes) = entry?;
        if let Ok(decoded) = decode_entry(v_bytes.value()) {
            count = count.saturating_add(1);
            total = total.saturating_add(decoded.size_bytes);
        }
    }
    Ok((count, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_key(recipe_byte: u8) -> CookKey {
        CookKey {
            recipe_hash: [recipe_byte; 32],
            target_triple: "x86_64-unknown-linux-gnu".into(),
            profile: "release".into(),
            channel: "1.94.1".into(),
            rustc_version: "rustc 1.94.1 (abcdef0)".into(),
        }
    }

    fn sample_entry(sha_byte: u8, size: u64, origin: Option<&str>) -> CookEntry {
        CookEntry {
            sha256: [sha_byte; 32],
            size_bytes: size,
            created_unix_ms: 1_700_000_000_000,
            last_used_unix_ms: 1_700_000_000_000,
            origin_url_normalized: origin.map(str::to_owned),
            cook_cmd_summary: "cook --release".into(),
        }
    }

    crate::timed_test!(round_trip_upsert_lookup, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        let key = sample_key(1);
        let entry = sample_entry(0xAA, 1024, Some("https://github.com/zackees/soldr"));
        upsert(&path, &key, &entry).expect("upsert");
        let got = lookup(&path, &key).expect("lookup");
        assert_eq!(got, Some(entry));
    });

    crate::timed_test!(lookup_returns_none_when_missing, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        let key = sample_key(2);
        assert_eq!(lookup(&path, &key).expect("lookup"), None);
    });

    crate::timed_test!(per_target_safety_isolates_rows, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        let mut a = sample_key(3);
        a.target_triple = "x86_64-unknown-linux-gnu".into();
        let mut b = sample_key(3);
        b.target_triple = "aarch64-apple-darwin".into();
        upsert(&path, &a, &sample_entry(0xAA, 100, None)).expect("upsert a");
        upsert(&path, &b, &sample_entry(0xBB, 200, None)).expect("upsert b");
        let got_a = lookup(&path, &a).expect("lookup a").expect("hit a");
        let got_b = lookup(&path, &b).expect("lookup b").expect("hit b");
        assert_eq!(got_a.sha256, [0xAA; 32]);
        assert_eq!(got_b.sha256, [0xBB; 32]);
        assert_eq!(got_a.size_bytes, 100);
        assert_eq!(got_b.size_bytes, 200);
    });

    crate::timed_test!(per_profile_safety_isolates_rows, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        let mut release = sample_key(4);
        release.profile = "release".into();
        let mut debug = sample_key(4);
        debug.profile = "dev".into();
        upsert(&path, &release, &sample_entry(0xCC, 500, None)).expect("upsert rel");
        upsert(&path, &debug, &sample_entry(0xDD, 600, None)).expect("upsert dev");
        assert_ne!(
            lookup(&path, &release).expect("a").expect("hit").sha256,
            lookup(&path, &debug).expect("b").expect("hit").sha256
        );
    });

    crate::timed_test!(
        drift_collects_other_recipe_hashes_with_same_origin_and_target,
        {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.redb");
            let origin = "https://github.com/zackees/soldr";
            let key_a = sample_key(5);
            let key_b = sample_key(6);
            let mut entry_a = sample_entry(0xA1, 10, Some(origin));
            entry_a.last_used_unix_ms = 100;
            let mut entry_b = sample_entry(0xB2, 20, Some(origin));
            entry_b.last_used_unix_ms = 200;
            upsert(&path, &key_a, &entry_a).expect("upsert a");
            upsert(&path, &key_b, &entry_b).expect("upsert b");
            let miss = sample_key(7);
            let drift = drift_recipe_hashes(&path, &miss, Some(origin), 10).expect("drift");
            assert_eq!(drift.len(), 2);
            assert_eq!(drift[0], [6u8; 32]);
            assert_eq!(drift[1], [5u8; 32]);
        }
    );

    crate::timed_test!(drift_skips_other_targets_and_other_origins, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        let origin = "https://github.com/zackees/soldr";
        let mut same_origin_diff_triple = sample_key(10);
        same_origin_diff_triple.target_triple = "aarch64-apple-darwin".into();
        let same_origin_diff_triple_entry = sample_entry(0x11, 1, Some(origin));
        let mut diff_origin = sample_key(11);
        diff_origin.target_triple = "x86_64-unknown-linux-gnu".into();
        let diff_origin_entry = sample_entry(0x22, 1, Some("https://github.com/other/repo"));
        upsert(
            &path,
            &same_origin_diff_triple,
            &same_origin_diff_triple_entry,
        )
        .expect("upsert sodt");
        upsert(&path, &diff_origin, &diff_origin_entry).expect("upsert do");
        let mut miss = sample_key(12);
        miss.target_triple = "x86_64-unknown-linux-gnu".into();
        let drift = drift_recipe_hashes(&path, &miss, Some(origin), 10).expect("drift");
        assert!(drift.is_empty());
    });

    crate::timed_test!(drift_returns_empty_without_origin_hint, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        upsert(
            &path,
            &sample_key(20),
            &sample_entry(0xEE, 1, Some("https://x")),
        )
        .expect("upsert");
        let miss = sample_key(21);
        assert!(drift_recipe_hashes(&path, &miss, None, 10)
            .expect("drift")
            .is_empty());
    });

    crate::timed_test!(touch_bumps_last_used_ms, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        let key = sample_key(30);
        let mut entry = sample_entry(0xFF, 1, None);
        entry.last_used_unix_ms = 1;
        upsert(&path, &key, &entry).expect("upsert");
        assert!(touch(&path, &[0xFF; 32], 9_999).expect("touch"));
        let got = lookup(&path, &key).expect("lookup").expect("hit");
        assert_eq!(got.last_used_unix_ms, 9_999);
    });

    crate::timed_test!(touch_returns_false_when_sha_unknown, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        assert!(!touch(&path, &[0x00; 32], 1).expect("touch"));
    });

    crate::timed_test!(stats_sums_entries_and_sizes, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        upsert(&path, &sample_key(40), &sample_entry(0x40, 1_024, None)).expect("a");
        upsert(&path, &sample_key(41), &sample_entry(0x41, 2_048, None)).expect("b");
        upsert(&path, &sample_key(42), &sample_entry(0x42, 4_096, None)).expect("c");
        let (count, total) = stats(&path).expect("stats");
        assert_eq!(count, 3);
        assert_eq!(total, 1_024 + 2_048 + 4_096);
    });

    crate::timed_test!(ensure_initialized_is_idempotent, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        ensure_initialized(&path).expect("init 1");
        ensure_initialized(&path).expect("init 2");
        let (count, total) = stats(&path).expect("stats");
        assert_eq!((count, total), (0, 0));
    });

    // Tag-byte hygiene: every value blob starts with 0x01.
    crate::timed_test!(value_blobs_carry_the_prost_tag, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        upsert(&path, &sample_key(50), &sample_entry(0x50, 1, None)).expect("upsert");
        let db = open_db(&path).expect("open");
        let txn = db.begin_read().expect("read");
        let table = txn.open_table(COOK_INDEX_V2).expect("v2");
        for entry in table.iter().expect("iter") {
            let (_, v) = entry.expect("entry");
            assert_eq!(v.value()[0], REDB_TAG_PROST);
        }
    });

    // migrate_drop_v1 evicts a populated v1 table on first
    // ensure_initialized call.
    crate::timed_test!(migration_drops_pre_580_cook_index_v1, {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("state.redb");
        // Seed a v1 row with arbitrary bytes — the format doesn't matter
        // for the migration; anything in cook_index_v1 must go.
        {
            let db = open_db(&path).expect("open");
            let txn = db.begin_write().expect("begin");
            {
                let mut table = txn.open_table(COOK_INDEX_V1).expect("v1");
                table
                    .insert(&[0x00u8, 0x01, 0x02][..], &[0xDE, 0xAD, 0xBE, 0xEF][..])
                    .expect("insert legacy");
            }
            txn.commit().expect("commit");
            drop(db);
        }
        // ensure_initialized drops the v1 table.
        ensure_initialized(&path).expect("ensure");
        // Confirm v1 is absent.
        let db = open_db(&path).expect("open");
        let txn = db.begin_read().expect("read");
        assert!(
            txn.open_table(COOK_INDEX_V1).is_err(),
            "cook_index_v1 must be dropped after migration"
        );
    });
}
