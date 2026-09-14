//! The durable cargo abort record written by `append_cargo_abort_log`.
//! Split out of `tests.rs`, which is over the 1,500-line ratchet (soldr#1966).

use super::*;

#[test]
fn cargo_abort_log_records_timeout_cleanup_and_recovery() {
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().to_path_buf());
    let cleanup = CargoAbortCleanupReport {
        orphan_rmetas_pruned: 2,
        incremental_dirs_removed: 1,
    };

    let path = append_cargo_abort_log(CargoAbortLogRequest {
        paths: &paths,
        session_id: 42,
        repo_root: Path::new("repo"),
        started_at_ms: 1_000,
        ended_at_ms: 2_500,
        args: &[
            String::from("build"),
            String::from("-p"),
            String::from("demo"),
        ],
        timeout: true,
        cargo_wait_timeout: Some(Duration::from_secs(30)),
        cleanup,
        message: "cargo timed out",
        auto_retry_planned: true,
    })
    .expect("append cargo abort log");

    assert_eq!(path, paths.cargo_abort_log());
    let log = std::fs::read_to_string(&path).expect("read cargo abort log");
    let lines: Vec<_> = log.lines().collect();
    assert_eq!(lines.len(), 1, "expected one jsonl record: {log}");
    let record: serde_json::Value =
        serde_json::from_str(lines[0]).expect("cargo abort log record is JSON");

    assert_eq!(record["schema_version"], serde_json::Value::from(2));
    assert_eq!(record["event"], serde_json::Value::from("cargo_abort"));
    assert_eq!(record["session_id"], serde_json::Value::from(42));
    assert_eq!(record["timeout"], serde_json::Value::from(true));
    assert_eq!(record["timeout_config"]["explicit"], true);
    assert_eq!(
        record["timeout_config"]["source"],
        CARGO_WAIT_TIMEOUT_ENV_VAR
    );
    assert_eq!(record["timeout_config"]["duration_secs"], 30);
    assert_eq!(record["auto_retry_planned"], serde_json::Value::from(true));
    assert_eq!(record["elapsed_ms"], serde_json::Value::from(1_500));
    assert_eq!(
        record["cleanup"]["orphan_rmetas_pruned"],
        serde_json::Value::from(2)
    );
    assert_eq!(
        record["cleanup"]["incremental_dirs_removed"],
        serde_json::Value::from(1)
    );
    assert_eq!(
        record["recovery"]["retry_with_zccache_disabled"]["env"]["ZCCACHE_DISABLE"],
        serde_json::Value::from("1")
    );
    assert_eq!(
        record["recovery"]["retry_with_zccache_disabled"]["argv"],
        serde_json::json!(["soldr", "cargo", "build", "-p", "demo"])
    );
    assert_eq!(
        record["recovery"]["clean_hint"],
        serde_json::json!({
            "env": {"ZCCACHE_DISABLE": "1"},
            "argv": ["soldr", "cargo", "clean", "-p", "<crate>"],
        })
    );
    // soldr#2424 / soldr#2777: the deprecated, hidden flag must not come back
    // through the structured record either.
    assert!(
        !record["recovery"].to_string().contains("--no-cache"),
        "recovery must not advise the deprecated --no-cache flag: {}",
        record["recovery"]
    );
    assert_eq!(
        record["recovery"]["inspect_logs"],
        serde_json::json!(["soldr", "logs", "paths"])
    );
}
