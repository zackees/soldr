//! Unit coverage split from `logs_cmd.rs` for the soldr#2493
//! 1,000-line production-source ceiling.

use super::*;

#[test]
fn build_log_paths_output_carries_schema_version_one() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let output = build_log_paths_output(&paths);
    assert_eq!(output.schema_version, 1);
    assert_eq!(output.root, tmp.path());
    assert!(!output.paths.is_empty(), "must include at least one entry");
}

#[test]
fn build_log_paths_output_names_embedded_zccache_paths() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let output = build_log_paths_output(&paths);
    let embedded_root = crate::zccache_embedded::embedded_cache_root(&paths);
    let versioned_root: zccache::core::NormalizedPath = embedded_root.clone().into();
    let versioned_root =
        zccache::core::config::effective_cache_root_from_top_level(&versioned_root);

    let root_entry = output
        .paths
        .iter()
        .find(|entry| entry.name == "zccache-embedded-cache-root")
        .expect("embedded cache root entry must exist");
    assert_eq!(root_entry.path, embedded_root);

    let logs_entry = output
        .paths
        .iter()
        .find(|entry| entry.name == "zccache-embedded-logs")
        .expect("embedded logs entry must exist");
    assert_eq!(logs_entry.path, versioned_root.join("logs").as_path());
    assert!(logs_entry.description.contains("compile_journal.jsonl"));

    let warnings_entry = output
        .paths
        .iter()
        .find(|entry| entry.name == "embedded-zccache-warning-logs")
        .expect("embedded warning log entry must exist");
    assert_eq!(
        warnings_entry.path,
        paths.cache.join("soldr-daemon").join("logs")
    );
    assert!(warnings_entry
        .description
        .contains("embedded-zccache.warn.log"));

    assert!(
        output
            .paths
            .iter()
            .all(|entry| entry.name != "zccache-private-daemon-roots"
                && entry.name != "zccache-default-session-logs"),
        "removed standalone/private zccache layouts must not be advertised"
    );
}

#[test]
fn build_log_paths_output_names_soldr_daemon_runtime() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let output = build_log_paths_output(&paths);
    let entry = output
        .paths
        .iter()
        .find(|e| e.name == "soldr-daemon-runtime")
        .expect("soldr-daemon-runtime entry must exist");
    let expected = tmp.path().join("runtime").join("soldr-daemon");
    assert_eq!(entry.path, expected);
}

#[test]
fn build_log_paths_output_names_cargo_abort_log() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let output = build_log_paths_output(&paths);
    let entry = output
        .paths
        .iter()
        .find(|e| e.name == "soldr-cargo-abort-log")
        .expect("soldr-cargo-abort-log entry must exist");
    let expected = tmp.path().join("logs").join("cargo-aborts.jsonl");
    assert_eq!(entry.path, expected);
    assert!(entry.description.contains("cargo front-door aborts"));
    let fallback = output
        .paths
        .iter()
        .find(|e| e.name == "soldr-compile-daemon-fallback-log")
        .expect("soldr-compile-daemon-fallback-log entry must exist");
    assert_eq!(
        fallback.path,
        tmp.path()
            .join("logs")
            .join("compile-daemon-fallbacks.jsonl")
    );
    assert!(fallback.description.contains("cache-bypass fallbacks"));
}

#[test]
fn build_log_paths_output_marks_missing_dirs() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let output = build_log_paths_output(&paths);
    // Fresh tmpdir → no soldr install → most entries should be
    // `exists = false`. The root itself exists (it's the tmpdir).
    let root_entry = output
        .paths
        .iter()
        .find(|e| e.name == "soldr-root")
        .expect("soldr-root entry must exist");
    assert!(root_entry.exists, "soldr-root should exist (tmpdir)");
    let zccache_entry = output
        .paths
        .iter()
        .find(|e| e.name == "zccache-embedded-cache-root")
        .expect("embedded cache root entry must exist");
    assert!(
        !zccache_entry.exists,
        "embedded cache root under a fresh tmpdir must NOT exist yet"
    );
}

#[test]
fn wrap_description_handles_long_text() {
    let lines = wrap_description("the quick brown fox jumps over the lazy dog", 12);
    // each line must be <= 12 chars (greedy fit; first word always
    // lands even if it overflows).
    for line in &lines {
        assert!(line.len() <= 12, "line too long: {line:?}");
    }
    // joined back must equal the original (modulo whitespace).
    let joined = lines.join(" ");
    assert_eq!(joined, "the quick brown fox jumps over the lazy dog");
}

#[test]
fn json_output_is_valid_json() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let output = build_log_paths_output(&paths);
    let s = serde_json::to_string(&output).expect("serialize");
    // Round-trip through serde_json::Value to confirm well-formed.
    let v: serde_json::Value = serde_json::from_str(&s).expect("re-parse");
    assert_eq!(v["schema_version"], serde_json::Value::from(1));
    let arr = v["paths"].as_array().expect("paths must be array");
    assert!(!arr.is_empty());
}

fn seeded_build(session_id: u64, started_at_ms: i64) -> BuildRecord {
    BuildRecord {
        session_id,
        repo_root: "/repo".into(),
        started_at_ms,
        ended_at_ms: Some(started_at_ms + 1_500),
        exit_code: Some(0),
        total_wall_ms: Some(1_500),
        crate_count: 2,
        slowest_crate_us: Some(900_000),
        slowest_crate_name: Some("slow-crate".into()),
        cache_summary: Some(BuildCacheSummary {
            hits: 8,
            misses: 2,
            non_cacheable: 1,
            errors: 0,
            compilations: 11,
            time_saved_ms: 750,
        }),
        log_paths: Some(BuildLogPaths {
            zccache_session_id: Some("session-1".into()),
            cache_dir: Some("/cache/zccache".into()),
            session_log_path: Some("/cache/zccache/logs/last-session.log".into()),
            journal_path: Some("/cache/zccache/logs/last-session.jsonl".into()),
            session_stats_path: Some("/cache/zccache/logs/last-session-stats.json".into()),
            compile_journal_path: Some("/cache/zccache/logs/compile_journal.jsonl".into()),
            archived_session_log_path: Some("/cache/zccache/history/1/last-session.log".into()),
            archived_journal_path: Some("/cache/zccache/history/1/last-session.jsonl".into()),
            archived_session_stats_path: Some(
                "/cache/zccache/history/1/last-session-stats.json".into(),
            ),
            archived_compile_journal_path: Some(
                "/cache/zccache/history/1/compile_journal.jsonl".into(),
            ),
            private_daemon_name: Some("soldr-dev-demo".into()),
        }),
        miss_reasons: vec![BuildMissReason {
            reason: "key_mismatch".into(),
            count: 2,
        }],
    }
}

#[test]
fn logs_list_requires_the_daemon_query() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let db_path = db::db_path(&paths);
    db::upsert_build(&db_path, &seeded_build(42, 1_000)).expect("upsert");

    let error = collect_logs_list_output_for_paths(&paths, 10)
        .expect_err("logs list must not open the daemon-owned database");
    assert!(
        error.to_string().contains("daemon"),
        "error should explain the daemon requirement: {error}"
    );
}

#[test]
fn logs_show_accepts_hex_prefix_and_lists_slow_compiles() {
    let session_id = 0xabc_def0_1234_u64;
    let record = resolve_launch_record(vec![seeded_build(session_id, 1_000)], "00000abc")
        .expect("prefix resolves");
    let events = vec![
        Event {
            ts_ms: 1_100,
            session_id: Some(session_id),
            kind: EventKind::CompileEnd,
            crate_name: Some("fast-crate".into()),
            duration_us: Some(250_000),
            target_dir: Some("/repo/target".into()),
            exit_code: None,
        },
        Event {
            ts_ms: 1_200,
            session_id: Some(session_id),
            kind: EventKind::CompileEnd,
            crate_name: Some("slow-crate".into()),
            duration_us: Some(2_500_000),
            target_dir: Some("/repo/target".into()),
            exit_code: None,
        },
    ];

    assert_eq!(record.session_id, session_id);
    let slow_compiles = slow_compile_events(&events, 10);
    assert_eq!(slow_compiles.len(), 2);
    assert_eq!(slow_compiles[0].crate_name.as_deref(), Some("slow-crate"));
}
