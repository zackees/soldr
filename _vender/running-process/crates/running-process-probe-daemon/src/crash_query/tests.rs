//! Tests for filtered crash history and cross-class rollups (#641).

use std::num::NonZeroU32;

use rusqlite::params;
use tempfile::TempDir;

use super::*;

fn limit(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).expect("test limit must be non-zero")
}

/// A store with a known set of crashes, seeded straight into SQLite.
///
/// Going through `record()` would require a real spool file per crash and
/// would make the timestamps whatever the clock said, which is the one thing
/// a time-window test cannot have.
struct Fixture {
    _dir: TempDir,
    store: CrashStore,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let store = CrashStore::open(
            &dir.path().join("crashes.db"),
            &dir.path().join("artifacts"),
        )
        .expect("open crash store");
        Self { _dir: dir, store }
    }

    /// Seed one crash row.
    #[allow(clippy::too_many_arguments)]
    fn seed(
        &self,
        app_class: &str,
        app_name: &str,
        instance: &str,
        signature: &str,
        crashed_at_ms: i64,
        pid: i64,
    ) {
        let conn = self.store.connection().lock().expect("store lock");
        conn.execute(
            "INSERT INTO crashes (app_class, app_name, app_version, instance_name, pid,
                                  creation_time_ms, cwd, signature, crashed_at_ms, exit_signal,
                                  report_json, artifact_path, artifact_bytes)
             VALUES (?1, ?2, '1.0', ?3, ?4, 1, '/work', ?5, ?6, 'SIGSEGV',
                     ?7, 'crash.json', 128)",
            params![
                app_class,
                app_name,
                instance,
                pid,
                signature,
                crashed_at_ms,
                // Deliberately sensitive: nothing this module returns may
                // carry it.
                r#"{"env":{"AWS_SECRET_ACCESS_KEY":"hunter2"}}"#,
            ],
        )
        .expect("seed crash row");
    }

    /// The canonical two-class, three-signature fixture.
    fn seeded() -> Self {
        let fixture = Self::new();
        fixture.seed("clud", "clud", "a", "SIGSEGV@parse", 1_000, 11);
        fixture.seed("clud", "clud", "b", "SIGSEGV@parse", 2_000, 12);
        fixture.seed("clud-worker", "worker", "a", "SIGSEGV@parse", 3_000, 13);
        fixture.seed("clud-worker", "worker", "a", "SIGABRT@assert", 4_000, 14);
        fixture.seed("other", "other", "a", "SIGILL@jit", 5_000, 15);
        fixture
    }

    fn classes_of(&self, filter: &CrashFilter) -> Vec<String> {
        let mut classes: Vec<String> = self
            .store
            .query(filter, limit(100))
            .expect("query")
            .into_iter()
            .map(|record| record.app_class)
            .collect();
        classes.sort();
        classes
    }
}

// --- filtering ------------------------------------------------------------

#[test]
fn an_exact_class_filter_returns_only_that_class() {
    let fixture = Fixture::seeded();
    let filter = CrashFilter {
        app_class: Some("clud".into()),
        ..CrashFilter::default()
    };
    assert_eq!(fixture.classes_of(&filter), vec!["clud", "clud"]);
}

#[test]
fn a_like_filter_sweeps_related_classes_together() {
    // The reason `app_class_like` exists: "all clud crashes" means the worker
    // too, and an exact match silently drops half the incident.
    let fixture = Fixture::seeded();
    let filter = CrashFilter {
        app_class_like: Some("clud%".into()),
        ..CrashFilter::default()
    };
    assert_eq!(
        fixture.classes_of(&filter),
        vec!["clud", "clud", "clud-worker", "clud-worker"]
    );
}

#[test]
fn like_wildcards_in_a_class_name_are_escapable() {
    // `_` is a single-character wildcard, so an unescaped `my_app` would also
    // match `myXapp`. Anything building a pattern from a literal class name
    // has to go through `escape_like`.
    let fixture = Fixture::new();
    fixture.seed("my_app", "my_app", "a", "sig", 1_000, 1);
    fixture.seed("myXapp", "myXapp", "a", "sig", 2_000, 2);

    let unescaped = CrashFilter {
        app_class_like: Some("my_app".into()),
        ..CrashFilter::default()
    };
    assert_eq!(
        fixture.classes_of(&unescaped).len(),
        2,
        "wildcard `_` matches both"
    );

    let escaped = CrashFilter {
        app_class_like: Some(escape_like("my_app")),
        ..CrashFilter::default()
    };
    assert_eq!(fixture.classes_of(&escaped), vec!["my_app"]);
}

#[test]
fn a_signature_filter_crosses_classes() {
    let fixture = Fixture::seeded();
    let filter = CrashFilter {
        signature: Some("SIGSEGV@parse".into()),
        ..CrashFilter::default()
    };
    assert_eq!(
        fixture.classes_of(&filter),
        vec!["clud", "clud", "clud-worker"]
    );
}

#[test]
fn the_time_window_is_half_open() {
    let fixture = Fixture::seeded();
    // [2000, 4000) — includes the crash at 2000, excludes the one at 4000.
    let filter = CrashFilter {
        since_unix_ms: Some(2_000),
        until_unix_ms: Some(4_000),
        ..CrashFilter::default()
    };
    let times: Vec<u64> = fixture
        .store
        .query(&filter, limit(100))
        .expect("query")
        .into_iter()
        .map(|record| record.crashed_at_ms)
        .collect();
    assert_eq!(times, vec![3_000, 2_000]);
}

#[test]
fn filters_combine_with_and() {
    let fixture = Fixture::seeded();
    let filter = CrashFilter {
        app_class_like: Some("clud%".into()),
        signature: Some("SIGABRT@assert".into()),
        ..CrashFilter::default()
    };
    assert_eq!(fixture.classes_of(&filter), vec!["clud-worker"]);
}

#[test]
fn records_come_back_newest_first() {
    let fixture = Fixture::seeded();
    let times: Vec<u64> = fixture
        .store
        .query(&CrashFilter::default(), limit(100))
        .expect("query")
        .into_iter()
        .map(|record| record.crashed_at_ms)
        .collect();
    assert_eq!(times, vec![5_000, 4_000, 3_000, 2_000, 1_000]);
}

// --- bounds ---------------------------------------------------------------

#[test]
fn the_limit_truncates_to_the_newest_page() {
    let fixture = Fixture::seeded();
    let records = fixture
        .store
        .query(&CrashFilter::default(), limit(2))
        .expect("query");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].crashed_at_ms, 5_000);
}

#[test]
fn an_oversized_limit_is_refused() {
    let fixture = Fixture::new();
    let err = fixture
        .store
        .query(&CrashFilter::default(), limit(MAX_CRASH_LIMIT + 1))
        .expect_err("an unbounded page must be refused");
    assert!(matches!(
        err,
        CrashStoreError::Query(CrashQueryError::LimitTooLarge { .. })
    ));
}

#[test]
fn an_inverted_window_is_refused_rather_than_returning_nothing() {
    // An empty result would be indistinguishable from "nothing crashed",
    // which is the most misleading answer this surface can give.
    let filter = CrashFilter {
        since_unix_ms: Some(5_000),
        until_unix_ms: Some(1_000),
        ..CrashFilter::default()
    };
    assert_eq!(
        filter.validate(),
        Err(CrashQueryError::EmptyWindow {
            since: 5_000,
            until: 1_000
        })
    );
}

#[test]
fn an_oversized_filter_string_is_refused() {
    let filter = CrashFilter {
        signature: Some("x".repeat(MAX_FILTER_BYTES + 1)),
        ..CrashFilter::default()
    };
    assert!(matches!(
        filter.validate(),
        Err(CrashQueryError::FilterTooLong {
            field: "signature",
            ..
        })
    ));
}

#[test]
fn a_filter_value_is_bound_not_concatenated() {
    // If any filter were interpolated into the SQL, this would be a syntax
    // error or worse. As a bound parameter it is simply a class nothing
    // matches.
    let fixture = Fixture::seeded();
    let filter = CrashFilter {
        app_class: Some("clud'; DROP TABLE crashes; --".into()),
        ..CrashFilter::default()
    };
    assert!(fixture
        .store
        .query(&filter, limit(10))
        .expect("query")
        .is_empty());
    // The table is still there, with everything in it.
    assert_eq!(
        fixture
            .store
            .query(&CrashFilter::default(), limit(100))
            .expect("query")
            .len(),
        5
    );
}

// --- stats ----------------------------------------------------------------

#[test]
fn stats_group_by_signature_most_frequent_first() {
    let fixture = Fixture::seeded();
    let stats = fixture.store.stats(&CrashFilter::default()).expect("stats");

    assert_eq!(stats.total, 5);
    assert_eq!(stats.distinct_classes, 3);
    assert_eq!(stats.first_unix_ms, 1_000);
    assert_eq!(stats.last_unix_ms, 5_000);

    let top = &stats.signatures[0];
    assert_eq!(top.signature, "SIGSEGV@parse");
    assert_eq!(top.count, 3);
    assert_eq!(top.first_unix_ms, 1_000);
    assert_eq!(top.last_unix_ms, 3_000);
    // The cross-class fact: this signature spans two classes, which is what
    // makes it worth looking at first.
    assert_eq!(top.app_classes, vec!["clud", "clud-worker"]);

    let signatures: Vec<&str> = stats
        .signatures
        .iter()
        .map(|s| s.signature.as_str())
        .collect();
    assert_eq!(
        signatures,
        vec!["SIGSEGV@parse", "SIGABRT@assert", "SIGILL@jit"]
    );
}

#[test]
fn stats_honour_the_same_filter_as_a_record_query() {
    let fixture = Fixture::seeded();
    let filter = CrashFilter {
        app_class_like: Some("clud%".into()),
        ..CrashFilter::default()
    };
    let stats = fixture.store.stats(&filter).expect("stats");
    assert_eq!(stats.total, 4);
    assert_eq!(stats.distinct_classes, 2);
    assert_eq!(
        stats.signatures.iter().map(|s| s.count).sum::<u64>(),
        stats.total
    );
}

#[test]
fn a_time_window_rollup_counts_only_that_window() {
    let fixture = Fixture::seeded();
    let stats = fixture
        .store
        .stats(&CrashFilter {
            since_unix_ms: Some(3_000),
            until_unix_ms: Some(6_000),
            ..CrashFilter::default()
        })
        .expect("stats");
    assert_eq!(stats.total, 3);
    assert_eq!(stats.first_unix_ms, 3_000);
    assert_eq!(stats.last_unix_ms, 5_000);
}

#[test]
fn the_total_is_not_capped_by_a_record_page() {
    // The reason `stats` exists at all. 40 crashes, a page of 10: counting
    // rows out of the page would report 10 and would report it confidently.
    let fixture = Fixture::new();
    for i in 0..40 {
        fixture.seed("clud", "clud", "a", "SIGSEGV@parse", 1_000 + i, i);
    }
    assert_eq!(
        fixture
            .store
            .query(&CrashFilter::default(), limit(10))
            .expect("query")
            .len(),
        10
    );
    assert_eq!(
        fixture
            .store
            .stats(&CrashFilter::default())
            .expect("stats")
            .total,
        40
    );
}

#[test]
fn stats_over_an_empty_match_set_are_zero_not_an_error() {
    let fixture = Fixture::seeded();
    let stats = fixture
        .store
        .stats(&CrashFilter {
            app_class: Some("nonexistent".into()),
            ..CrashFilter::default()
        })
        .expect("stats");
    assert_eq!(stats, CrashStats::default());
}

// --- redaction ------------------------------------------------------------

#[test]
fn query_results_never_carry_the_inline_report() {
    // Every seeded row's `report_json` holds a fake secret. The record type
    // has no field to put it in, and the SELECT does not read the column —
    // this asserts the second part, so adding the column back to
    // `RECORD_COLUMNS` breaks a test rather than shipping the secret.
    assert!(
        !RECORD_COLUMNS.contains("report_json"),
        "the inline crash report must never be selected onto a query surface"
    );

    let fixture = Fixture::seeded();
    let records = fixture
        .store
        .query(&CrashFilter::default(), limit(100))
        .expect("query");
    let rendered = format!("{records:?}");
    assert!(!rendered.contains("AWS_SECRET_ACCESS_KEY"));
    assert!(!rendered.contains("hunter2"));
}

#[test]
fn stats_never_carry_per_crash_detail() {
    let fixture = Fixture::seeded();
    let rendered = format!(
        "{:?}",
        fixture.store.stats(&CrashFilter::default()).expect("stats")
    );
    assert!(!rendered.contains("hunter2"));
    assert!(!rendered.contains("/work"), "no cwd in an aggregate");
}
