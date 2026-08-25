//! Integration coverage for the `[cook]` auto-GC eviction pass
//! (issue #589).
//!
//! Each case constructs a tempdir-backed `SoldrPaths`, seeds the
//! `cook_index_v2` table + matching on-disk `<sha>.tar.zst` files,
//! runs `cook_evict_pass` directly, and asserts the post-state via
//! `cook_index::iter_entries` + filesystem checks. Runs on every
//! platform — no Docker harness required because no daemon spawns.

use soldr_cli::cache_lib::cook_archive::{artifact_path_for_sha, cook_cache_dir};
use soldr_cli::cache_lib::cook_gc::{cook_evict_pass, cook_evict_pass_with_absolute_age};
use soldr_cli::cache_lib::cook_index::{self, CookEntry, CookKey};
use soldr_cli::cache_lib::state_db_path;
use soldr_cli::core::{CookConfig, SoldrPaths};

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

const GIB: u64 = 1024 * 1024 * 1024;
const MS_PER_DAY: u64 = 24 * 60 * 60 * 1000;
const SECS_PER_DAY: u64 = 24 * 60 * 60;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn make_key(seed: u8) -> CookKey {
    CookKey {
        recipe_hash: [seed; 32],
        target_triple: "x86_64-unknown-linux-gnu".into(),
        profile: "release".into(),
        channel: "1.94.1".into(),
        rustc_version: "rustc 1.94.1 (test)".into(),
    }
}

fn make_entry(sha_byte: u8, size: u64, last_used: u64, origin: Option<&str>) -> CookEntry {
    CookEntry {
        sha256: [sha_byte; 32],
        size_bytes: size,
        created_unix_ms: 1_700_000_000_000,
        last_used_unix_ms: last_used,
        origin_url_normalized: origin.map(str::to_owned),
        cook_cmd_summary: "cook --release".into(),
        branch_name: None,
        compile_duration_ms: 60_000,
        save_elapsed_ms: 5_000,
    }
}

/// Seed `(CookKey, CookEntry)` pairs into the cook index + write a
/// tiny placeholder file at the canonical artifact path so the
/// eviction pass has something to unlink. The on-disk file is NOT
/// sized to match `entry.size_bytes` — the eviction algorithm only
/// reads sizes from the index, never from the filesystem.
fn seed_entries(paths: &SoldrPaths, entries: &[(u8, CookEntry)]) -> Vec<PathBuf> {
    let db = state_db_path(paths);
    let cook_dir = cook_cache_dir(paths);
    std::fs::create_dir_all(&cook_dir).expect("mkdir cook");
    let mut artifacts = Vec::new();
    for (seed, entry) in entries {
        cook_index::upsert(&db, &make_key(*seed), entry).expect("upsert");
        let art = artifact_path_for_sha(&cook_dir, &entry.sha256);
        std::fs::write(&art, b"x").expect("write artifact placeholder");
        artifacts.push(art);
    }
    artifacts
}

fn fresh_paths(label: &str) -> (TempDir, SoldrPaths) {
    let dir = TempDir::new().expect("tempdir");
    let label = label.replace([' ', '/'], "_");
    let root = dir.path().join(format!("soldr-{label}"));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let paths = SoldrPaths::with_root(root);
    paths.ensure_dirs().expect("ensure_dirs");
    (dir, paths)
}

#[test]
fn per_origin_top_n_preserved_under_size_pressure() {
    let (_guard, paths) = fresh_paths("per-origin");
    let now = now_ms();
    let origin = Some("https://github.com/zackees/soldr");
    let entries: Vec<(u8, CookEntry)> = (0..5u8)
        .map(|i| {
            let last = now - ((5 - i as u64) * 1_000);
            (i + 1, make_entry(0x10 + i, GIB, last, origin))
        })
        .collect();
    seed_entries(&paths, &entries);
    // Total = 5 GiB; cap = 3 GiB; keep_per_origin = 3 → evict the
    // 2 oldest, leaving the 3 most recently used.
    let cfg = CookConfig {
        max_total_gb: 3,
        max_age_days: 365 * 10,
        keep_per_origin: 3,
        ..CookConfig::default()
    };
    let report = cook_evict_pass(&paths, &cfg);
    assert_eq!(report.protected, 3);
    assert_eq!(report.size_evicted, 2);
    assert_eq!(report.time_evicted, 0);
    let remaining = cook_index::iter_entries(&state_db_path(&paths)).expect("iter");
    assert_eq!(remaining.len(), 3);
    let cook_dir = cook_cache_dir(&paths);
    assert!(!artifact_path_for_sha(&cook_dir, &[0x10; 32]).exists());
    assert!(!artifact_path_for_sha(&cook_dir, &[0x11; 32]).exists());
    assert!(artifact_path_for_sha(&cook_dir, &[0x12; 32]).exists());
    assert!(artifact_path_for_sha(&cook_dir, &[0x13; 32]).exists());
    assert!(artifact_path_for_sha(&cook_dir, &[0x14; 32]).exists());
}

#[test]
fn full_maintenance_absolute_age_overrides_per_origin_protection() {
    let (_guard, paths) = fresh_paths("absolute-age");
    let old = make_entry(
        0x91,
        100,
        now_ms() - 31 * MS_PER_DAY,
        Some("https://github.com/zackees/abandoned"),
    );
    let artifacts = seed_entries(&paths, &[(1, old)]);
    let cfg = CookConfig {
        max_total_gb: 200,
        max_age_days: 365,
        keep_per_origin: 3,
        ..CookConfig::default()
    };

    let report = cook_evict_pass_with_absolute_age(
        &paths,
        &cfg,
        Some(Duration::from_secs(30 * SECS_PER_DAY)),
    );
    assert_eq!(report.protected, 0);
    assert_eq!(report.time_evicted, 1);
    assert!(!artifacts[0].exists());
}

#[test]
fn time_bound_evicts_entries_older_than_max_age() {
    let (_guard, paths) = fresh_paths("time-bound");
    let now = now_ms();
    let stale_a = make_entry(0xA1, 100, now - 50 * MS_PER_DAY, None);
    let stale_b = make_entry(0xA2, 200, now - 40 * MS_PER_DAY, None);
    let fresh = make_entry(0xA3, 50, now - MS_PER_DAY, None);
    seed_entries(
        &paths,
        &[
            (1, stale_a.clone()),
            (2, stale_b.clone()),
            (3, fresh.clone()),
        ],
    );
    let cfg = CookConfig {
        max_total_gb: 1024, // size cap inert
        max_age_days: 30,
        keep_per_origin: 0, // disable per-origin protection
        ..CookConfig::default()
    };
    let report = cook_evict_pass(&paths, &cfg);
    assert_eq!(report.time_evicted, 2);
    assert_eq!(report.size_evicted, 0);
    assert_eq!(report.bytes_freed, 100 + 200);
    let remaining = cook_index::iter_entries(&state_db_path(&paths)).expect("iter");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].1.sha256, [0xA3; 32]);
}

#[test]
fn size_cap_evicts_oldest_by_last_used() {
    let (_guard, paths) = fresh_paths("size-cap");
    let now = now_ms();
    let entries: Vec<(u8, CookEntry)> = (0..4u8)
        .map(|i| {
            let last = now - ((4 - i as u64) * 1_000);
            (i + 1, make_entry(0x40 + i, GIB, last, None))
        })
        .collect();
    seed_entries(&paths, &entries);
    // Total 4 GiB, cap 2 GiB → evict the 2 oldest.
    let cfg = CookConfig {
        max_total_gb: 2,
        max_age_days: 365 * 10,
        keep_per_origin: 0,
        ..CookConfig::default()
    };
    let report = cook_evict_pass(&paths, &cfg);
    assert_eq!(report.size_evicted, 2);
    assert_eq!(report.time_evicted, 0);
    let remaining = cook_index::iter_entries(&state_db_path(&paths)).expect("iter");
    let mut surviving: Vec<u8> = remaining.iter().map(|(_, e)| e.sha256[0]).collect();
    surviving.sort();
    assert_eq!(surviving, vec![0x42, 0x43]);
}

#[test]
fn quarantine_files_cleaned_only_on_time_bound() {
    let (_guard, paths) = fresh_paths("quarantine");
    let cook_dir = cook_cache_dir(&paths);
    std::fs::create_dir_all(&cook_dir).expect("mkdir cook");
    let old = cook_dir.join("aaaaaaaa.tar.zst.quarantine");
    let young = cook_dir.join("bbbbbbbb.tar.zst.quarantine");
    std::fs::write(&old, b"old").expect("old");
    std::fs::write(&young, b"young").expect("young");
    let stale = SystemTime::now() - Duration::from_secs(40 * SECS_PER_DAY);
    filetime::set_file_mtime(&old, filetime::FileTime::from_system_time(stale))
        .expect("set old mtime");
    let cfg = CookConfig {
        max_total_gb: 0,
        max_age_days: 30,
        keep_per_origin: 0,
        ..CookConfig::default()
    };
    let report = cook_evict_pass(&paths, &cfg);
    assert_eq!(report.quarantine_evicted, 1);
    assert!(!old.exists());
    assert!(young.exists());
}

#[test]
fn mixed_origin_workload() {
    let (_guard, paths) = fresh_paths("mixed-origin");
    let now = now_ms();
    let origin_a = Some("https://github.com/a/x");
    let origin_b = Some("https://github.com/b/y");
    let mut seed_byte: u8 = 1;
    for (group, count, base_sha) in [
        (origin_a, 3u8, 0xA0u8),
        (origin_b, 2u8, 0xB0u8),
        (None, 2u8, 0xC0u8),
    ] {
        for i in 0..count {
            let sha = base_sha + i;
            let entry = make_entry(sha, GIB, now - ((count - i) as u64 * 1_000), group);
            seed_entries(&paths, &[(seed_byte, entry)]);
            seed_byte = seed_byte.checked_add(1).expect("seed overflow");
        }
    }
    // 7 GiB total; cap 3 GiB → evict 4. keep_per_origin = 1 protects
    // exactly the 3 most-recently-used entries (one per group).
    let cfg = CookConfig {
        max_total_gb: 3,
        max_age_days: 365 * 10,
        keep_per_origin: 1,
        ..CookConfig::default()
    };
    let report = cook_evict_pass(&paths, &cfg);
    assert_eq!(report.protected, 3);
    assert_eq!(report.size_evicted, 4);
    assert_eq!(report.time_evicted, 0);
    let remaining = cook_index::iter_entries(&state_db_path(&paths)).expect("iter");
    assert_eq!(remaining.len(), 3);
    let groups: std::collections::HashSet<Option<String>> = remaining
        .iter()
        .map(|(_, e)| e.origin_url_normalized.clone())
        .collect();
    assert_eq!(groups.len(), 3);
}

#[test]
fn cook_gc_rejects_a_linked_cross_product_root() {
    let (guard, paths) = fresh_paths("linked-root");
    let external = guard.path().join("other-product");
    std::fs::create_dir_all(&external).unwrap();
    let sentinel = external.join("sentinel");
    std::fs::write(&sentinel, b"keep").unwrap();
    let cook_dir = cook_cache_dir(&paths);
    if cook_dir.exists() {
        std::fs::remove_dir_all(&cook_dir).unwrap();
    }
    soldr_platform::fs::links::create(
        external.to_str().expect("UTF-8 external path"),
        &cook_dir,
        true,
    )
    .expect("create cook cache link");
    let report = cook_evict_pass(&paths, &CookConfig::default());
    assert_eq!(report.errors, 1);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
}

#[test]
fn cook_unlink_failure_retains_the_index_for_retry() {
    let (_guard, paths) = fresh_paths("unlink-failure");
    let entry = make_entry(0x77, 1, 0, None);
    let artifact = seed_entries(&paths, &[(1, entry)])[0].clone();
    soldr_cli::core::replace_file_with_dir(&artifact, Duration::from_secs(10))
        .expect("swap the artifact file for an unlinkable directory");
    let report = cook_evict_pass(
        &paths,
        &CookConfig {
            max_age_days: 1,
            keep_per_origin: 0,
            ..CookConfig::default()
        },
    );
    assert_eq!(report.errors, 1);
    assert_eq!(report.time_evicted, 0);
    assert_eq!(
        cook_index::iter_entries(&state_db_path(&paths))
            .unwrap()
            .len(),
        1,
        "failed unlink must remain indexed for a future retry"
    );
    assert!(artifact.is_dir());
}
