//! soldr#3365 regression suite: retired-embedded-zccache-store expiry must be
//! **per file**, never keyed on the newest mtime anywhere in the tree.
//!
//! The fixture in [`host_shaped_stores`] mirrors the retired `v1.13.22` store
//! observed on a dev host on 2026-09-25: 499 GB, 1,463,494 entries, with the
//! newest entry (`.writer.lock`, touched by the last service shutdown) only
//! 27 h old. Under the current whole-tree newest-mtime gate
//! (`latest_tree_mtime` in `zccache_embedded_legacy.rs`) that one fresh
//! bookkeeping write pinned the entire store, however old the bulk of its
//! artifacts were.
//!
//! Every test below is expected **RED** until per-file expiry lands: `A`,
//! `B`, `C`, `D`, `E` assert the required per-file behaviour and currently
//! fail because the whole store is kept or removed as one unit. `F` is a
//! guard on already-correct behaviour (the current version store is never
//! touched) and is expected to pass today and after the fix.

use super::*;
use std::time::Duration;

pub(crate) struct HostShapedStores {
    pub retired: PathBuf,
    pub current: PathBuf,
    pub cold_artifacts: Vec<PathBuf>,
    pub warm_artifacts: Vec<PathBuf>,
    pub current_files: Vec<PathBuf>,
    pub cold_artifact_bytes: u64,
}

fn hours(count: f64) -> Duration {
    Duration::from_secs_f64(count * 3600.0)
}

fn set_mtime(path: &std::path::Path, time: SystemTime) {
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time))
        .unwrap_or_else(|error| panic!("set mtime for {}: {error}", path.display()));
}

/// Build a host-shaped pair of retired (`v1.13.22`) and current
/// (`versioned_subdir()`) embedded-zccache stores under `paths`, all ages
/// relative to `now`.
pub(crate) fn host_shaped_stores(paths: &SoldrPaths, now: SystemTime) -> HostShapedStores {
    let embedded_root = embedded_cache_root(paths);
    let retired = embedded_root.join("v1.13.22");
    assert_ne!(
        "v1.13.22",
        zccache::core::config::versioned_subdir(),
        "fixture retired-store name must not collide with the current version"
    );
    let current = embedded_root.join(zccache::core::config::versioned_subdir());

    let cold_payload = vec![0u8; 4096];
    let mut cold_artifacts = Vec::new();
    let mut cold_artifact_bytes: u64 = 0;

    // 64 shard dirs (00..3f hex), 30 files each = 1,920 cold artifacts, all
    // older than the 72 h pressure gate (4-20 days old).
    let mut index = 0usize;
    for shard in 0u32..64 {
        let shard_dir = retired.join("artifacts").join(format!("{shard:02x}"));
        std::fs::create_dir_all(&shard_dir).expect("cold shard dir");
        for i in 0..30u32 {
            let file = shard_dir.join(format!("{i:04}.bin"));
            std::fs::write(&file, &cold_payload).expect("cold artifact");
            let age_hours = 96.0 + ((index % 17) as f64) * 24.0;
            set_mtime(&file, now - hours(age_hours));
            cold_artifact_bytes += cold_payload.len() as u64;
            cold_artifacts.push(file);
            index += 1;
        }
    }
    assert_eq!(cold_artifacts.len(), 1_920);

    // 12 warm artifacts in three of those same shards, all younger than 72 h.
    let mut warm_artifacts = Vec::new();
    let warm_ages_hours = [10.0, 25.0, 47.0, 60.0];
    for shard_name in ["00", "1a", "3f"] {
        let shard_dir = retired.join("artifacts").join(shard_name);
        std::fs::create_dir_all(&shard_dir).expect("warm shard dir");
        for k in 0..4u32 {
            let file = shard_dir.join(format!("warm-{k}.bin"));
            std::fs::write(&file, b"warm").expect("warm artifact");
            set_mtime(&file, now - hours(warm_ages_hours[k as usize]));
            warm_artifacts.push(file);
        }
    }
    assert_eq!(warm_artifacts.len(), 12);

    // Bookkeeping mirroring the real store — not asserted on individually
    // except where a test says so.
    let bookkeeping: &[(&str, f64)] = &[
        ("tmp/stale.tmp", 19.0 * 24.0),
        (".disk-maintenance-last-full-v1", 14.0 * 24.0),
        ("compiler_hash.bin", 8.0 * 24.0),
        ("metadata.bin", 8.0 * 24.0),
        (".index.bin.tmp-1649967", 69.0),
        ("system_includes.bin", 47.0),
        ("logs/zccache.log", 33.0),
        ("depgraph/graph.bin", 27.0),
        ("index.bin", 27.0),
        (ZCCACHE_WRITER_LOCK_FILE, 27.0),
    ];
    for (rel, age_hours) in bookkeeping {
        let file = retired.join(rel);
        std::fs::create_dir_all(file.parent().expect("bookkeeping parent"))
            .expect("bookkeeping parent dir");
        std::fs::write(&file, b"bookkeeping").expect("bookkeeping file");
        set_mtime(&file, now - hours(*age_hours));
    }
    let staging = retired.join("staging");
    std::fs::create_dir_all(&staging).expect("staging dir (empty)");

    // Something wrote into the tree 27 h ago: set every directory in the
    // retired store fresh. This is what proves directory freshness must be
    // ignored — under the current whole-tree gate it alone pins everything.
    let mut retired_dirs = vec![retired.clone(), retired.join("artifacts")];
    for shard in 0u32..64 {
        retired_dirs.push(retired.join("artifacts").join(format!("{shard:02x}")));
    }
    for extra in ["logs", "depgraph", "staging", "tmp"] {
        retired_dirs.push(retired.join(extra));
    }
    for dir in &retired_dirs {
        set_mtime(dir, now - hours(27.0));
    }

    // Current store: never touched by the sweep, however old its files are.
    let mut current_files = Vec::new();
    let current_shard = current.join("artifacts").join("aa");
    std::fs::create_dir_all(&current_shard).expect("current shard dir");
    for i in 0..200u32 {
        let file = current_shard.join(format!("{i:04}.bin"));
        std::fs::write(&file, &cold_payload).expect("current artifact");
        set_mtime(&file, now - hours(40.0 * 24.0));
        current_files.push(file);
    }
    let current_index = current.join("index.bin");
    std::fs::write(&current_index, b"current index").expect("current index");
    set_mtime(&current_index, now - hours(40.0 * 24.0));
    current_files.push(current_index);

    HostShapedStores {
        retired,
        current,
        cold_artifacts,
        warm_artifacts,
        current_files,
        cold_artifact_bytes,
    }
}

#[test]
fn host_shaped_retired_store_expires_every_artifact_older_than_the_pressure_gate() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let fixture = host_shaped_stores(&paths, now);

    let report = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);

    let cold_survivors: Vec<&PathBuf> = fixture
        .cold_artifacts
        .iter()
        .filter(|path| path.exists())
        .collect();
    assert!(
        cold_survivors.is_empty(),
        "{} of {} cold artifacts survived the sweep (expected each to expire on its own \
         mtime); first few: {:?}; report={report:?}",
        cold_survivors.len(),
        fixture.cold_artifacts.len(),
        cold_survivors.iter().take(5).collect::<Vec<_>>(),
    );

    let missing_warm: Vec<&PathBuf> = fixture
        .warm_artifacts
        .iter()
        .filter(|path| !path.exists())
        .collect();
    assert!(
        missing_warm.is_empty(),
        "warm artifacts must survive: {missing_warm:?}; report={report:?}"
    );

    let missing_current: Vec<&PathBuf> = fixture
        .current_files
        .iter()
        .filter(|path| !path.exists())
        .collect();
    assert!(
        missing_current.is_empty(),
        "current-store files must never be touched: {missing_current:?}; report={report:?}"
    );

    assert_eq!(report.failed, 0, "report={report:?}");
    assert_eq!(report.live_retained, 0, "report={report:?}");
    assert!(
        report.bytes_reclaimed >= fixture.cold_artifact_bytes,
        "expected at least {} bytes reclaimed, got {}: report={report:?}",
        fixture.cold_artifact_bytes,
        report.bytes_reclaimed,
    );
    assert!(
        fixture.retired.is_dir(),
        "the store directory must remain because warm artifacts survive: report={report:?}"
    );
}

#[test]
fn directory_freshness_does_not_pin_older_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let retired = embedded_cache_root(&paths).join("v0.0.1");
    let shard = retired.join("artifacts").join("ab");
    std::fs::create_dir_all(&shard).expect("shard dir");

    let old1 = shard.join("old-1.bin");
    let old2 = shard.join("old-2.bin");
    let fresh = shard.join("fresh.bin");
    for file in [&old1, &old2] {
        std::fs::write(file, b"payload").expect("old artifact");
        set_mtime(file, now - hours(10.0 * 24.0));
    }
    std::fs::write(&fresh, b"payload").expect("fresh artifact");
    set_mtime(&fresh, now - hours(1.0));

    for dir in [&shard, &retired.join("artifacts"), &retired] {
        set_mtime(dir, now);
    }

    let report = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);

    assert!(
        !old1.exists(),
        "old-1.bin must expire despite fresh parent dirs: report={report:?}"
    );
    assert!(
        !old2.exists(),
        "old-2.bin must expire despite fresh parent dirs: report={report:?}"
    );
    assert!(fresh.exists(), "fresh.bin must survive: report={report:?}");
}

/// The core pinning-loop regression: a writer that keeps touching the store
/// (a stray old binary, a pinned CI job, a mounted Docker harness) must never
/// be able to hold the whole tree young forever. Each pass re-writes fresh
/// bookkeeping and one fresh artifact, then every artifact ever planted is
/// checked against its *own* age.
#[test]
fn a_writer_that_keeps_writing_cannot_pin_the_store() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let t0 = SystemTime::now();
    let retired = embedded_cache_root(&paths).join("v0.0.1");
    let artifacts = retired.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");

    let mut planted: Vec<(PathBuf, SystemTime)> = Vec::new();
    for k in 1..=20u64 {
        let file = artifacts.join(format!("seed-{k}.bin"));
        let mtime = t0 - (hours(k as f64 * 12.0) + Duration::from_secs(30 * 60));
        std::fs::write(&file, b"payload").expect("seed artifact");
        set_mtime(&file, mtime);
        planted.push((file, mtime));
    }

    for d in 0..6u64 {
        let now_d = t0 + Duration::from_secs(d * 24 * 60 * 60);

        let written = artifacts.join(format!("written-{d}.bin"));
        let written_mtime = now_d - Duration::from_secs(60);
        std::fs::write(&written, b"payload").expect("written artifact");
        set_mtime(&written, written_mtime);
        planted.push((written, written_mtime));

        for name in ["index.bin", ZCCACHE_WRITER_LOCK_FILE] {
            let file = retired.join(name);
            std::fs::write(&file, b"payload").expect("bookkeeping rewrite");
            set_mtime(&file, now_d - Duration::from_secs(60));
        }
        set_mtime(&retired, now_d - Duration::from_secs(60));
        set_mtime(&artifacts, now_d - Duration::from_secs(60));

        let report =
            sweep_legacy_cache_roots(&paths, now_d, crate::cache_lib::gc_policy::STALENESS_GATE);

        for (file, mtime) in &planted {
            let expected_alive = now_d
                .duration_since(*mtime)
                .map(|age| age < crate::cache_lib::gc_policy::STALENESS_GATE)
                .unwrap_or(true);
            assert_eq!(
                file.exists(),
                expected_alive,
                "pass d={d}: {} expected exists={expected_alive} (mtime={mtime:?}, \
                 now_d={now_d:?}); report={report:?}",
                file.display(),
            );
        }
    }
}

#[test]
fn a_store_with_no_artifacts_left_is_removed_entirely() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let retired = embedded_cache_root(&paths).join("v0.0.1");
    let artifacts = retired.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");

    for i in 0..50u32 {
        let file = artifacts.join(format!("old-{i:04}.bin"));
        std::fs::write(&file, b"payload").expect("old artifact");
        set_mtime(&file, now - hours(10.0 * 24.0));
    }

    for rel in [
        "index.bin",
        ZCCACHE_WRITER_LOCK_FILE,
        "depgraph/graph.bin",
        "logs/zccache.log",
    ] {
        let file = retired.join(rel);
        std::fs::create_dir_all(file.parent().expect("bookkeeping parent"))
            .expect("bookkeeping parent dir");
        std::fs::write(&file, b"payload").expect("bookkeeping file");
        set_mtime(&file, now - hours(2.0));
    }
    let staging = retired.join("staging");
    std::fs::create_dir_all(&staging).expect("staging dir");

    for dir in [
        &retired,
        &artifacts,
        &retired.join("depgraph"),
        &retired.join("logs"),
        &staging,
    ] {
        set_mtime(dir, now - hours(2.0));
    }

    let report = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);

    assert!(
        !retired.exists(),
        "a store with only bookkeeping left must be removed entirely: report={report:?}"
    );
    assert_eq!(report.failed, 0, "report={report:?}");
}

#[test]
fn a_held_writer_lock_defers_expiry_until_the_service_exits() {
    use fs2::FileExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let retired = embedded_cache_root(&paths).join("v0.0.1");
    let artifacts = retired.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifacts dir");

    let mut cold = Vec::new();
    for i in 0..10u32 {
        let file = artifacts.join(format!("cold-{i:02}.bin"));
        std::fs::write(&file, b"payload").expect("cold artifact");
        set_mtime(&file, now - hours(10.0 * 24.0));
        cold.push(file);
    }
    let warm = artifacts.join("warm.bin");
    std::fs::write(&warm, b"payload").expect("warm artifact");
    set_mtime(&warm, now - hours(1.0));

    let lock_path = retired.join(ZCCACHE_WRITER_LOCK_FILE);
    let lock = std::fs::File::create(&lock_path).expect("writer lock file");
    lock.try_lock_exclusive().expect("acquire writer lock");

    let held = sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);
    assert_eq!(held.live_retained, 1, "{held:?}");
    assert_eq!(held.failed, 0, "{held:?}");
    for file in &cold {
        assert!(
            file.exists(),
            "cold artifact must be untouched while the writer lock is held: {}",
            file.display()
        );
    }
    assert!(warm.exists(), "warm artifact must survive: {held:?}");

    drop(lock);
    let released =
        sweep_legacy_cache_roots(&paths, now, crate::cache_lib::gc_policy::STALENESS_GATE);
    assert_eq!(released.live_retained, 0, "{released:?}");
    for file in &cold {
        assert!(
            !file.exists(),
            "cold artifact must expire once the lock is released: {}",
            file.display()
        );
    }
    assert!(
        warm.exists(),
        "warm artifact must survive after lock release: {released:?}"
    );
}

/// Guard: the current version's store must never be touched, however old its
/// files are. Expected GREEN today and after the fix.
#[test]
fn the_current_store_is_never_touched_however_old() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join(".soldr"));
    let now = SystemTime::now();
    let fixture = host_shaped_stores(&paths, now);

    let _report = sweep_legacy_cache_roots(&paths, now, Duration::ZERO);

    for file in &fixture.current_files {
        assert!(
            file.exists(),
            "current-store file must never be swept: {}",
            file.display()
        );
    }
}
