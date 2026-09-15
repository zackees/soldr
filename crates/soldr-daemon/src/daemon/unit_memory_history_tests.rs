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
