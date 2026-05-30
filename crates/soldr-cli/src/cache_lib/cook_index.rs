//! Cross-repo shared `soldr cook` artifact index (issue #576, meta
//! #579). PR 1 of 3: the **dormant** index schema and operations.
//!
//! This module owns a new redb table inside the shared
//! `~/.soldr/state.redb` and gives the daemon a per-request-open
//! surface to look up, record, touch, and stat cook artifacts. Heavy
//! I/O (tar+zstd:19 packing) lives in the PR 2 `soldr cook` worker;
//! this module never reads or writes the artifact bytes themselves —
//! it only tracks where they are and how big they are.
//!
//! Key tuple (bincode-serialized blob, used as the redb key):
//!
//! ```text
//! (recipe_hash: [u8; 32], target_triple, profile, channel, rustc_version)
//! ```
//!
//! Cross-target sharing is structurally impossible by including
//! triple/profile/channel/rustc in the key — there is no code path
//! that can relax this without changing the schema.
//!
//! ## Wire compatibility
//!
//! Table name is `cook_index_v1`. A future schema change MUST land as
//! `cook_index_v2` rather than mutating the v1 table, so old daemons
//! and old soldr binaries that resume an upgraded state.redb fall
//! back gracefully.

use crate::cache_lib::target_registry::RegistryError;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// redb table name. Bumping this for schema breaks (rare) — additive
/// changes use the existing key/value bincode schema.
const COOK_INDEX_V1: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cook_index_v1");

/// Lookup key for the cook artifact index. Cross-target sharing is
/// forbidden by including triple/profile/channel/rustc in the key —
/// two builds that differ on any of these resolve to different rows
/// regardless of recipe_hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct CookKey {
    pub recipe_hash: [u8; 32],
    pub target_triple: String,
    pub profile: String,
    pub channel: String,
    pub rustc_version: String,
}

/// Stored value for a cook artifact entry. PR 2 writes these via
/// `CookRecord`; PR 3 reads them via `CookLookup` + `CookTouch`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

fn init_table(db: &Database) -> Result<(), RegistryError> {
    let txn = db.begin_write()?;
    {
        let _ = txn.open_table(COOK_INDEX_V1)?;
    }
    txn.commit()?;
    Ok(())
}

/// Idempotent: open the `state.redb` file and ensure `cook_index_v1`
/// exists. Other helpers call this internally; callers (e.g. daemon
/// startup) can use it to eager-create the table.
pub fn ensure_initialized(db_path: &Path) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    init_table(&db)
}

fn encode_key(key: &CookKey) -> Result<Vec<u8>, RegistryError> {
    bincode::serialize(key).map_err(bincode_err)
}

fn decode_entry(bytes: &[u8]) -> Result<CookEntry, RegistryError> {
    bincode::deserialize(bytes).map_err(bincode_err)
}

fn encode_entry(entry: &CookEntry) -> Result<Vec<u8>, RegistryError> {
    bincode::serialize(entry).map_err(bincode_err)
}

fn decode_key(bytes: &[u8]) -> Result<CookKey, RegistryError> {
    bincode::deserialize(bytes).map_err(bincode_err)
}

/// Upsert a cook artifact entry. PR 2's `soldr cook` worker calls
/// this via the daemon `CookRecord` IPC after the tarball has been
/// written to `~/.soldr/cache/cook/<sha256>.tar.zst`.
pub fn upsert(db_path: &Path, key: &CookKey, entry: &CookEntry) -> Result<(), RegistryError> {
    let db = open_db(db_path)?;
    init_table(&db)?;
    let key_bytes = encode_key(key)?;
    let value_bytes = encode_entry(entry)?;
    let txn = db.begin_write()?;
    {
        let mut table = txn.open_table(COOK_INDEX_V1)?;
        table.insert(key_bytes.as_slice(), value_bytes.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// Look up a single entry by the full key tuple. PR 3's cargo-
/// front-door pre-flight calls this via the daemon `CookLookup` IPC.
pub fn lookup(db_path: &Path, key: &CookKey) -> Result<Option<CookEntry>, RegistryError> {
    let db = open_db(db_path)?;
    init_table(&db)?;
    let key_bytes = encode_key(key)?;
    let txn = db.begin_read()?;
    let table = txn.open_table(COOK_INDEX_V1)?;
    let Some(row) = table.get(key_bytes.as_slice())? else {
        return Ok(None);
    };
    Ok(Some(decode_entry(row.value())?))
}

/// Drift diagnostic: given a (missing) lookup key, scan the table for
/// other rows whose `(origin_url_normalized, target_triple, profile,
/// channel, rustc_version)` match — but whose `recipe_hash` differs.
/// Returns up to `limit` previous recipe hashes, useful as the
/// "previous_origin_recipe_hashes" diagnostic in `CookMiss`.
///
/// When `origin_url_normalized` is `None`, drift diagnostic is
/// skipped (returns empty) — without an origin hint we have no way to
/// know which other rows "belong to the same repo".
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
    init_table(&db)?;
    let txn = db.begin_read()?;
    let table = txn.open_table(COOK_INDEX_V1)?;
    let mut out: Vec<([u8; 32], u64)> = Vec::new();
    for entry in table.iter()? {
        let (k_bytes, v_bytes) = entry?;
        let stored_key = match decode_key(k_bytes.value()) {
            Ok(k) => k,
            Err(_) => continue,
        };
        if stored_key.recipe_hash == miss_key.recipe_hash {
            continue;
        }
        if stored_key.target_triple != miss_key.target_triple
            || stored_key.profile != miss_key.profile
            || stored_key.channel != miss_key.channel
            || stored_key.rustc_version != miss_key.rustc_version
        {
            continue;
        }
        let stored_entry = match decode_entry(v_bytes.value()) {
            Ok(e) => e,
            Err(_) => continue,
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

/// Bump the `last_used_unix_ms` field for the entry whose
/// `sha256` matches. Fire-and-forget by PR 3: a touch failure must
/// never affect callers. Returns `Ok(true)` when a row was bumped,
/// `Ok(false)` when no matching row exists.
///
/// Implementation note: this is O(N) over the index because the
/// primary key is `CookKey`, not `sha256`. PR 1 ships dormant and PR
/// 3's pre-flight is the only `CookTouch` caller; a secondary index
/// can be added later if measurements show it matters.
pub fn touch(db_path: &Path, sha256: &[u8; 32], now_unix_ms: u64) -> Result<bool, RegistryError> {
    let db = open_db(db_path)?;
    init_table(&db)?;
    let mut updates: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    {
        let txn = db.begin_read()?;
        let table = txn.open_table(COOK_INDEX_V1)?;
        for entry in table.iter()? {
            let (k_bytes, v_bytes) = entry?;
            let mut value = match decode_entry(v_bytes.value()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if &value.sha256 == sha256 {
                value.last_used_unix_ms = now_unix_ms;
                let new_v = encode_entry(&value)?;
                updates.push((k_bytes.value().to_vec(), new_v));
            }
        }
    }
    if updates.is_empty() {
        return Ok(false);
    }
    let txn = db.begin_write()?;
    {
        let mut table = txn.open_table(COOK_INDEX_V1)?;
        for (k, v) in &updates {
            table.insert(k.as_slice(), v.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(true)
}

/// Aggregate statistics surfaced via `Status` and `soldr daemon
/// status`: returns `(entry_count, total_size_bytes)`.
pub fn stats(db_path: &Path) -> Result<(u64, u64), RegistryError> {
    let db = open_db(db_path)?;
    init_table(&db)?;
    let txn = db.begin_read()?;
    let table = txn.open_table(COOK_INDEX_V1)?;
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
        // Same recipe hash, different target triple — must NOT collide.
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

            // Two prior entries, same origin/triple/profile/channel/rustc,
            // but different recipe hashes.
            let key_a = sample_key(5);
            let key_b = sample_key(6);
            let mut entry_a = sample_entry(0xA1, 10, Some(origin));
            entry_a.last_used_unix_ms = 100;
            let mut entry_b = sample_entry(0xB2, 20, Some(origin));
            entry_b.last_used_unix_ms = 200;
            upsert(&path, &key_a, &entry_a).expect("upsert a");
            upsert(&path, &key_b, &entry_b).expect("upsert b");

            // Lookup for a different recipe hash (miss). Drift should
            // return both prior hashes, newest-first.
            let miss = sample_key(7);
            let drift = drift_recipe_hashes(&path, &miss, Some(origin), 10).expect("drift");
            assert_eq!(drift.len(), 2);
            assert_eq!(drift[0], [6u8; 32]); // b.last_used=200 → newest first
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
}
