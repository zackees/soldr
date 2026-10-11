//! RED -> GREEN coverage for the soldr#3152 per-unit memory history.

use super::*;
use crate::cache_lib::state_store::open_state_db_in_memory;

const MIB: u64 = 1024 * 1024;

#[test]
fn a_recorded_unit_round_trips() {
    let db = open_state_db_in_memory().expect("in-memory state db");
    record_in(
        &db,
        "zccache/0123456789abcdef",
        90 * MIB,
        1_356 * MIB,
        1_000,
    )
    .expect("record");
    let unit = lookup_in(&db, "zccache/0123456789abcdef")
        .expect("lookup")
        .expect("unit present");
    assert_eq!(unit.peak_rss_bytes, 90 * MIB);
    assert_eq!(unit.tree_peak_rss_bytes, 1_356 * MIB);
    assert_eq!(unit.updated_ms, 1_000);
    assert_eq!(unit.samples, 1);
}

#[test]
fn the_latest_measurement_replaces_the_stored_peak_and_counts_samples() {
    // The owner's decision: admit on the unit's *last* measured peak, so a unit
    // that got cheaper is not held to an old, larger figure.
    let db = open_state_db_in_memory().expect("in-memory state db");
    record_in(&db, "soldr_cli/aaaa", 100 * MIB, 5_000 * MIB, 1_000).expect("first");
    record_in(&db, "soldr_cli/aaaa", 80 * MIB, 1_400 * MIB, 2_000).expect("second");
    let unit = lookup_in(&db, "soldr_cli/aaaa")
        .expect("lookup")
        .expect("present");
    assert_eq!(unit.peak_rss_bytes, 80 * MIB);
    assert_eq!(unit.tree_peak_rss_bytes, 1_400 * MIB);
    assert_eq!(unit.updated_ms, 2_000);
    assert_eq!(unit.samples, 2);
}

#[test]
fn an_unseen_unit_has_no_history() {
    let db = open_state_db_in_memory().expect("in-memory state db");
    assert!(lookup_in(&db, "never/seen").expect("lookup").is_none());
}

#[test]
fn rows_are_tagged_prost_and_untagged_bytes_are_refused() {
    let db = open_state_db_in_memory().expect("in-memory state db");
    record_in(&db, "anyhow/bbbb", 10 * MIB, 12 * MIB, 5).expect("record");
    let raw: Vec<u8> = db
        .query_row(
            "SELECT value FROM unit_memory_history WHERE key = ?1",
            ["anyhow/bbbb"],
            |row| row.get(0),
        )
        .expect("raw row");
    assert_eq!(raw.first(), Some(&crate::core::wire::REDB_TAG_PROST));

    db.execute(
        "INSERT INTO unit_memory_history(key, value) VALUES(?1, ?2)",
        rusqlite::params!["untagged/cccc", vec![0x08u8, 0x01]],
    )
    .expect("insert untagged");
    assert!(
        lookup_in(&db, "untagged/cccc").is_err(),
        "an untagged row must be refused"
    );
}

#[test]
fn load_all_reads_every_recorded_unit() {
    let db = open_state_db_in_memory().expect("in-memory state db");
    record_in(&db, "a/1", 1, 2, 10).expect("a");
    record_in(&db, "b/2", 3, 4, 20).expect("b");
    let mut all = load_all_in(&db).expect("load all");
    all.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].0, "a/1");
    assert_eq!(all[0].1.tree_peak_rss_bytes, 2);
    assert_eq!(all[1].0, "b/2");
    assert_eq!(all[1].1.peak_rss_bytes, 3);
}

#[tokio::test]
async fn the_recorder_answers_lookups_immediately_and_persists_on_flush() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("state.sqlite3");

    let history = UnitMemoryHistory::start(db_path.clone());
    history.record("zccache/abcd", 90 * MIB, 1_356 * MIB);
    // Admission reads the in-memory map, so a lookup must not wait for SQLite.
    let unit = history
        .lookup("zccache/abcd")
        .expect("recorded unit visible at once");
    assert_eq!(unit.tree_peak_rss_bytes, 1_356 * MIB);
    history.flush().await.expect("flush");

    // A fresh daemon generation warms its map from the table.
    let restarted = UnitMemoryHistory::start(db_path);
    restarted.ready().await.expect("warm load");
    let unit = restarted
        .lookup("zccache/abcd")
        .expect("persisted across restart");
    assert_eq!(unit.peak_rss_bytes, 90 * MIB);
    assert_eq!(unit.tree_peak_rss_bytes, 1_356 * MIB);
    assert_eq!(unit.samples, 1);
}

#[tokio::test]
async fn an_all_zero_measurement_is_not_recorded() {
    // A cache hit spawns no compiler, so zccache reports no memory. That is an
    // absence of a measurement, not a zero-byte unit.
    let temp = tempfile::tempdir().expect("tempdir");
    let history = UnitMemoryHistory::start(temp.path().join("state.sqlite3"));
    history.record("hit/eeee", 0, 0);
    history.flush().await.expect("flush");
    assert!(history.lookup("hit/eeee").is_none());
}

#[tokio::test]
async fn a_spawn_instant_reading_is_not_remembered() {
    // soldr#3152 two-pass data: compiles shorter than ~one watchdog tick
    // recorded only the sample taken at spawn (36-40 KiB for units that really
    // peak at 1-144 MiB). Such a reading is not a measurement of the unit.
    let temp = tempfile::tempdir().expect("tempdir");
    let history = UnitMemoryHistory::start(temp.path().join("state.sqlite3"));
    history.record("tempfile/a1ec", 40 * 1024, 40 * 1024);
    history.flush().await.expect("flush");
    assert!(history.lookup("tempfile/a1ec").is_none());
}

#[tokio::test]
async fn a_spawn_instant_reading_never_overwrites_a_real_peak() {
    let temp = tempfile::tempdir().expect("tempdir");
    let history = UnitMemoryHistory::start(temp.path().join("state.sqlite3"));
    history.record("running_process/1643", 487 * MIB, 500 * MIB);
    history.record("running_process/1643", 40 * 1024, 40 * 1024);
    history.flush().await.expect("flush");
    let unit = history
        .lookup("running_process/1643")
        .expect("real peak kept");
    assert_eq!(unit.tree_peak_rss_bytes, 500 * MIB);
    assert_eq!(unit.samples, 1);
}

fn insert_garbage(db: &Connection, key: &str) {
    db.execute(
        "INSERT INTO unit_memory_history(key, value) VALUES(?1, ?2)",
        rusqlite::params![key, vec![0xFFu8, 0x00, 0x13]],
    )
    .expect("insert garbage");
}

#[tokio::test]
async fn a_garbage_row_does_not_discard_the_valid_history() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("state.sqlite3");
    {
        let db = crate::cache_lib::state_store::open_state_db(&path).expect("open");
        record_in(&db, "good/0001", 100 * MIB, 2_000 * MIB, 1_000).expect("record");
        insert_garbage(&db, "bad/0002");
    }
    let history = UnitMemoryHistory::start(path);
    history.ready().await.unwrap();
    let unit = history.lookup("good/0001").expect("valid row survives");
    assert_eq!(unit.peak_rss_bytes, 100 * MIB);
    assert!(history.lookup("bad/0002").is_none());
}

#[test]
fn load_all_skips_an_undecodable_row() {
    let db = open_state_db_in_memory().expect("in-memory state db");
    record_in(&db, "good/0001", 1, 2, 10).expect("good");
    insert_garbage(&db, "bad/0002");
    let all = load_all_in(&db).expect("load all");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].0, "good/0001");
}

#[test]
fn record_overwrites_an_undecodable_row() {
    let db = open_state_db_in_memory().expect("in-memory state db");
    insert_garbage(&db, "bad/0002");
    record_in(&db, "bad/0002", 10 * MIB, 20 * MIB, 5).expect("record over garbage");
    let unit = lookup_in(&db, "bad/0002")
        .expect("lookup")
        .expect("present");
    assert_eq!(unit.samples, 1);
    assert_eq!(unit.peak_rss_bytes, 10 * MIB);
}

fn seed_trusted_row(db_path: &std::path::Path, key: &str, samples: u64) {
    let db = crate::cache_lib::state_store::open_state_db(db_path).expect("open");
    for i in 0..samples {
        record_in(&db, key, 900 * MIB, 4_000 * MIB, i64::try_from(i).unwrap()).expect("seed");
    }
}

/// soldr#3686: a record that lands before the warm load must not reset the
/// stored sample count below `MIN_TRUSTED_SAMPLES`. On a current-thread
/// runtime the owner task cannot run until the test awaits, so `record`
/// deterministically precedes the load.
#[tokio::test]
async fn a_record_during_the_warm_load_keeps_the_stored_sample_count() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("state.sqlite3");
    seed_trusted_row(&db_path, "heavy/abcd", 5);

    let history = UnitMemoryHistory::start(db_path);
    history.record("heavy/abcd", 100 * MIB, 200 * MIB);
    history.ready().await.expect("warm load");
    let unit = history.lookup("heavy/abcd").expect("unit");
    assert!(unit.samples >= 6, "samples reset to {}", unit.samples);
    assert_eq!(unit.tree_peak_rss_bytes, 4_000 * MIB);
}

/// soldr#3686: admission's bounded wait sees the trusted history right after
/// a restart instead of an empty map.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_lookup_waits_for_the_warm_load() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("state.sqlite3");
    seed_trusted_row(&db_path, "heavy/abcd", 5);

    let history = UnitMemoryHistory::start(db_path);
    let unit = history
        .lookup_after_load("heavy/abcd", std::time::Duration::from_secs(30))
        .expect("trusted history visible to admission after restart");
    assert_eq!(unit.samples, 5);
}
