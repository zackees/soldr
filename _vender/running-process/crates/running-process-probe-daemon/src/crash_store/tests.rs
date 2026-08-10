//! Tests for durable crash storage, retention, and fetch pinning.
//!
//! Split out of the module itself to keep that file under the repo's
//! per-file size guard. The coverage is unchanged.

use super::*;
use running_process::broker::secure_dir::private_dir_permissions_are_private;
use running_process_probe::crash::spool::{
    encode, CrashFrame, CrashMetadata, CrashModule, CrashThread, RawCrashReport,
};

fn report(index: u64) -> RawCrashReport {
    RawCrashReport {
        pid: 123,
        tid: 456,
        fault_code: test_fault_code(),
        fault_address: 0xdead,
        crash_unix_ms: unix_millis().saturating_add(index),
        metadata: CrashMetadata {
            app_class: "compiler".into(),
            app_name: "frontend".into(),
            app_version: "1.2.3".into(),
            instance_name: "ci".into(),
            creation_time_ms: 888,
            cwd: "/workspace".into(),
        },
        modules: vec![CrashModule {
            identity: "fixture.exe".into(),
        }],
        threads: vec![
            CrashThread {
                os_tid: 456,
                frames: vec![
                    CrashFrame {
                        module_index: Some(0),
                        relative_address: 0x1000,
                    },
                    CrashFrame {
                        module_index: Some(0),
                        relative_address: 0x2000,
                    },
                ],
            },
            CrashThread {
                os_tid: 789,
                frames: vec![CrashFrame {
                    module_index: Some(0),
                    relative_address: 0x3000,
                }],
            },
        ],
        raw_context: vec![0xaa, 0xbb],
        truncated: false,
    }
}

fn open_store(root: &Path, policy: CleanupPolicy) -> CrashStore {
    CrashStore::open_with_policy(
        &root.join("crashes.sqlite3"),
        &root.join("artifacts"),
        policy,
    )
    .unwrap()
}

fn unbounded_policy() -> CleanupPolicy {
    CleanupPolicy {
        max_age: Duration::ZERO,
        keep_last_n_per_app: 0,
        max_total_artifact_bytes: 0,
        max_rows: 0,
        max_single_artifact_bytes: DEFAULT_MAX_SINGLE_BYTES,
    }
}

fn worker_binary() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let candidate = path.join(format!(
        "running-process-probe-worker{}",
        std::env::consts::EXE_SUFFIX
    ));
    if candidate.is_file() {
        return Some(candidate);
    }
    assert!(
        std::env::var_os("GITHUB_ACTIONS").is_none(),
        "worker binary missing at {} in CI",
        candidate.display()
    );
    None
}

#[cfg(windows)]
fn test_fault_code() -> i64 {
    0xC000_0005u32 as i32 as i64
}

#[cfg(unix)]
fn test_fault_code() -> i64 {
    libc::SIGSEGV as i64
}

#[test]
fn persists_across_reopen_with_all_tags() {
    let root = tempfile::tempdir().unwrap();
    let first = open_store(root.path(), unbounded_policy());
    let inserted = first.record(&report(1)).unwrap();
    drop(first);

    let reopened = open_store(root.path(), unbounded_policy());
    let rows = reopened.query_by_class("compiler", 10).unwrap();
    assert_eq!(rows, vec![inserted.clone()]);
    assert_eq!(inserted.app_name, "frontend");
    assert_eq!(inserted.pid, 123);
    assert_eq!(inserted.creation_time_ms, 888);
    assert_eq!(inserted.cwd, "/workspace");
    assert_eq!(inserted.signature.len(), 64);
    assert!(inserted.artifact_path.exists());
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn non_unicode_artifact_root_round_trips_losslessly() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let parent = tempfile::tempdir().unwrap();
    let artifacts = parent
        .path()
        .join(OsString::from_vec(b"crashes-\xff".to_vec()));
    let database = parent.path().join("crashes.sqlite3");
    let first = CrashStore::open_with_policy(&database, &artifacts, unbounded_policy()).unwrap();
    let inserted = first.record(&report(1)).unwrap();
    assert!(inserted.artifact_path.exists());
    drop(first);

    let reopened = CrashStore::open_with_policy(&database, &artifacts, unbounded_policy()).unwrap();
    let row = reopened.get(inserted.id).unwrap().unwrap();
    assert_eq!(row.artifact_path, inserted.artifact_path);
    assert!(row.artifact_path.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_unicode_edge_artifact_root_round_trips() {
    let parent = tempfile::tempdir().unwrap();
    let artifacts = parent.path().join("crashes-e\u{301}-雪");
    let database = parent.path().join("crashes.sqlite3");
    let first = CrashStore::open_with_policy(&database, &artifacts, unbounded_policy()).unwrap();
    let inserted = first.record(&report(1)).unwrap();
    assert!(inserted.artifact_path.exists());
    drop(first);

    let reopened = CrashStore::open_with_policy(&database, &artifacts, unbounded_policy()).unwrap();
    let row = reopened.get(inserted.id).unwrap().unwrap();
    assert_eq!(row.artifact_path, inserted.artifact_path);
    assert!(row.artifact_path.exists());
}

#[test]
fn same_pid_with_different_creation_time_is_distinct() {
    let root = tempfile::tempdir().unwrap();
    let store = open_store(root.path(), unbounded_policy());
    let first = store.record(&report(1)).unwrap();
    let mut reused = report(2);
    reused.metadata.creation_time_ms += 1;
    let second = store.record(&reused).unwrap();
    assert_ne!(first.id, second.id);
    assert_ne!(first.creation_time_ms, second.creation_time_ms);
    assert_eq!(store.query_by_class("compiler", 10).unwrap().len(), 2);
}

#[test]
fn worker_availability_does_not_change_the_stable_signature() {
    let root = tempfile::tempdir().unwrap();
    let store = open_store(root.path(), unbounded_policy());
    let symbols = r#"{"threads":[{"os_tid":456,"frames":[
        {"function":"crash_here"},{"function":"main"}]}]}"#;
    let symbolized = store
        .record_with_symbol_report(&report(1), Some(symbols))
        .unwrap();
    let degraded = store.record(&report(2)).unwrap();
    assert_eq!(symbolized.signature, degraded.signature);
}

#[test]
fn gc_prunes_oldest_but_never_an_in_use_artifact() {
    let root = tempfile::tempdir().unwrap();
    let policy = CleanupPolicy {
        keep_last_n_per_app: 2,
        ..unbounded_policy()
    };
    let store = open_store(root.path(), policy);
    let oldest = store.record(&report(1)).unwrap();
    let lease = store.begin_fetch(oldest.id).unwrap().unwrap();
    let second = store.record(&report(2)).unwrap();
    let third = store.record(&report(3)).unwrap();
    let fourth = store.record(&report(4)).unwrap();

    store.gc().unwrap();
    assert!(lease.path().exists());
    assert!(store.get(oldest.id).unwrap().is_some());
    assert!(store.get(second.id).unwrap().is_none());
    assert!(store.get(third.id).unwrap().is_some());
    assert!(store.get(fourth.id).unwrap().is_some());
    drop(lease);
    store.gc().unwrap();
    assert!(store.get(oldest.id).unwrap().is_none());
    assert!(!oldest.artifact_path.exists());
}

#[test]
fn artifacts_and_directory_are_owner_only() {
    let root = tempfile::tempdir().unwrap();
    let store = open_store(root.path(), unbounded_policy());
    let _artifact_path = store.record(&report(1)).unwrap().artifact_path;
    assert!(private_dir_permissions_are_private(&store.artifacts_dir).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(_artifact_path).unwrap().permissions().mode() & 0o077,
            0
        );
    }
}

#[test]
fn reopen_reconciles_missing_and_orphan_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let store = open_store(root.path(), unbounded_policy());
    let row = store.record(&report(1)).unwrap();
    fs::remove_file(&row.artifact_path).unwrap();
    let orphan = store
        .artifacts_dir
        .join("crash-1-00000000000000000000000000000000.json");
    fs::write(&orphan, b"orphan").unwrap();
    let unknown = store.artifacts_dir.join("user-notes.txt");
    fs::write(&unknown, b"preserve").unwrap();
    drop(store);

    let reopened = open_store(root.path(), unbounded_policy());
    let row = reopened.get(row.id).unwrap().unwrap();
    assert!(row.artifact_path.as_os_str().is_empty());
    assert_eq!(row.artifact_bytes, 0);
    assert!(!orphan.exists());
    assert_eq!(fs::read(unknown).unwrap(), b"preserve");
}

#[test]
fn reconciliation_never_unlinks_a_tampered_external_path() {
    let root = tempfile::tempdir().unwrap();
    let store = open_store(root.path(), unbounded_policy());
    let row = store.record(&report(1)).unwrap();
    let external = root.path().join("must-survive.txt");
    fs::write(&external, b"owner data").unwrap();
    store
        .conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE crashes SET artifact_path = ?1 WHERE id = ?2",
            params![external.to_string_lossy(), row.id],
        )
        .unwrap();
    drop(store);

    let reopened = open_store(root.path(), unbounded_policy());
    assert_eq!(
        fs::read(&external).unwrap(),
        b"owner data",
        "a corrupted DB path must never extend cleanup outside artifacts"
    );
    assert!(reopened
        .get(row.id)
        .unwrap()
        .unwrap()
        .artifact_path
        .as_os_str()
        .is_empty());
}

#[test]
fn oversized_artifact_is_rejected_without_filesystem_debris() {
    let root = tempfile::tempdir().unwrap();
    let policy = CleanupPolicy {
        max_single_artifact_bytes: 1,
        ..unbounded_policy()
    };
    let store = open_store(root.path(), policy);
    assert!(matches!(
        store.record(&report(1)),
        Err(CrashStoreError::ArtifactTooLarge { .. })
    ));
    assert!(fs::read_dir(&store.artifacts_dir).unwrap().next().is_none());
    assert!(store.query_by_class("compiler", 10).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn artifact_root_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    fs::create_dir(&target).unwrap();
    let link = root.path().join("artifacts");
    symlink(&target, &link).unwrap();
    let error = CrashStore::open(&root.path().join("crashes.sqlite3"), &link)
        .expect_err("symlink must be rejected");
    assert!(matches!(error, CrashStoreError::Io(_)));
}

#[test]
fn concurrent_writers_are_serialized_by_sqlite() {
    let root = tempfile::tempdir().unwrap();
    let first = open_store(root.path(), unbounded_policy());
    let second = open_store(root.path(), unbounded_policy());
    std::thread::scope(|scope| {
        scope.spawn(|| {
            first.record(&report(1)).unwrap();
        });
        scope.spawn(|| {
            second.record(&report(2)).unwrap();
        });
    });
    assert_eq!(first.query_by_class("compiler", 10).unwrap().len(), 2);
}

#[test]
fn concurrent_open_preserves_a_live_session_pin() {
    let root = tempfile::tempdir().unwrap();
    let first = open_store(root.path(), unbounded_policy());
    let row = first.record(&report(1)).unwrap();
    let lease = first.begin_fetch(row.id).unwrap().unwrap();
    let policy = CleanupPolicy {
        keep_last_n_per_app: 1,
        ..unbounded_policy()
    };
    let second = open_store(root.path(), policy);
    second.record(&report(2)).unwrap();
    assert!(second.get(row.id).unwrap().is_some());
    assert_eq!(lease.file().metadata().unwrap().len(), row.artifact_bytes);
    drop(lease);
    second.gc().unwrap();
    assert!(second.get(row.id).unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn fetch_guard_reads_the_validated_handle_after_path_replacement() {
    use std::io::Read as _;

    let root = tempfile::tempdir().unwrap();
    let store = open_store(root.path(), unbounded_policy());
    let row = store.record(&report(1)).unwrap();
    let lease = store.begin_fetch(row.id).unwrap().unwrap();
    let moved = store.artifacts_dir.join("original-held-open.json");
    fs::rename(&row.artifact_path, &moved).unwrap();
    fs::write(&row.artifact_path, b"replacement").unwrap();

    let mut opened = lease.file().try_clone().unwrap();
    let mut contents = String::new();
    opened.read_to_string(&mut contents).unwrap();
    assert!(contents.contains("\"running-process.crash.v2\""));
    assert_ne!(contents, "replacement");
}

#[test]
fn concurrent_open_cannot_remove_an_inflight_publication() {
    let root = tempfile::tempdir().unwrap();
    let first = open_store(root.path(), unbounded_policy());
    let db = root.path().join("crashes.sqlite3");
    let artifacts = root.path().join("artifacts");
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            for index in 0..20 {
                first.record(&report(index)).unwrap();
            }
        });
        let opener = scope.spawn(|| {
            for _ in 0..20 {
                let opened =
                    CrashStore::open_with_policy(&db, &artifacts, unbounded_policy()).unwrap();
                assert!(opened
                    .query_by_class("compiler", 100)
                    .unwrap()
                    .iter()
                    .all(|row| row.artifact_path.exists()));
            }
        });
        writer.join().unwrap();
        opener.join().unwrap();
    });
    let rows = first.query_by_class("compiler", 100).unwrap();
    assert_eq!(rows.len(), 20);
    assert!(rows.iter().all(|row| row.artifact_path.exists()));
}

#[test]
fn stale_session_pins_are_recovered_on_open() {
    let root = tempfile::tempdir().unwrap();
    let store = open_store(root.path(), unbounded_policy());
    let row = store.record(&report(1)).unwrap();
    {
        let conn = store.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO crash_store_sessions (
                 session_id, pid, process_start_ms, boot_id
             ) VALUES ('stale', ?1, 1, ?2)",
            params![i64::from(u32::MAX), store.session.boot_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO crash_fetch_pins (crash_id, session_id, pin_count)
             VALUES (?1, 'stale', 1)",
            [row.id],
        )
        .unwrap();
        conn.execute("UPDATE crashes SET refcount = 1 WHERE id = ?1", [row.id])
            .unwrap();
    }
    drop(store);

    let reopened = open_store(root.path(), unbounded_policy());
    let refcount: i64 = reopened
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT refcount FROM crashes WHERE id = ?1",
            [row.id],
            |sql_row| sql_row.get(0),
        )
        .unwrap();
    assert_eq!(refcount, 0);
}

#[test]
fn additive_migration_preserves_a_development_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("crashes.sqlite3");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE crashes (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             app_class TEXT NOT NULL DEFAULT ''
         );
         INSERT INTO crashes (app_class) VALUES ('legacy');",
    )
    .unwrap();
    drop(conn);

    let store = open_store(root.path(), unbounded_policy());
    let legacy = store.query_by_class("legacy", 10).unwrap();
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0].pid, 0);
    let inserted = store.record(&report(1)).unwrap();
    assert_eq!(store.get(inserted.id).unwrap(), Some(inserted));
}

#[test]
fn pre_registration_record_is_ingested_when_daemon_appears() {
    let root = tempfile::tempdir().unwrap();
    let spool = root.path().join("spool");
    let reports = root.path().join("reports");
    ensure_private_dir(&spool).unwrap();
    let pending = spool.join("before-daemon.rpcrash");
    fs::write(&pending, encode(&report(1))).unwrap();

    let paths = ingest_pending(&spool, &reports).unwrap();
    assert_eq!(paths.len(), 1);
    assert!(!pending.exists());
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
    assert_eq!(json["app_class"], "compiler");
    assert_eq!(json["creation_time_ms"], 888);
    assert_eq!(json["cwd"], "/workspace");
    assert_eq!(json["all_threads"].as_array().unwrap().len(), 2);
    assert_eq!(json["fault_address"], "0xdead");
    assert_eq!(json["raw_context_hex"], "aabb");
}

#[test]
fn spool_retry_is_idempotent_but_a_distinct_source_is_not_deduplicated() {
    let root = tempfile::tempdir().unwrap();
    let spool = root.path().join("spool");
    let reports = root.path().join("reports");
    ensure_private_dir(&spool).unwrap();
    let bytes = encode(&report(1));
    let pending = spool.join("same-source.rpcrash");
    fs::write(&pending, bytes).unwrap();
    ingest_pending(&spool, &reports).unwrap();

    // Simulate a crash after DB commit but before source unlink: the same
    // stable source filename and content must return the original row.
    fs::write(&pending, bytes).unwrap();
    ingest_pending(&spool, &reports).unwrap();
    let store = CrashStore::open(&root.path().join("crashes.sqlite3"), &reports).unwrap();
    assert_eq!(store.query_by_class("compiler", 10).unwrap().len(), 1);

    // Two legitimate crashes can have identical bounded content. Their
    // independently pre-created spool identities keep them distinct.
    fs::write(spool.join("another-source.rpcrash"), bytes).unwrap();
    ingest_pending(&spool, &reports).unwrap();
    assert_eq!(store.query_by_class("compiler", 10).unwrap().len(), 2);
}

#[test]
fn s7_spool_is_routed_through_the_real_s8_worker() {
    let Some(worker) = worker_binary() else {
        eprintln!("skipping: worker binary not built");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let spool = root.path().join("spool");
    ensure_private_dir(&spool).unwrap();
    let store = open_store(root.path(), unbounded_policy());
    let pending = spool.join("real-worker.rpcrash");
    fs::write(&pending, encode(&report(1))).unwrap();

    let paths = ingest_pending_with_store_and_worker(&spool, &store, Some(&worker)).unwrap();
    assert_eq!(paths.len(), 1);
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
    assert!(
        json["symbolized"]["threads"].is_array(),
        "worker report missing from durable artifact: {json}"
    );
    assert_eq!(json["symbolized"]["threads"][0]["os_tid"], 456);
    assert_eq!(
        json["symbolized"]["threads"][0]["frames"][0]["module"],
        "fixture.exe"
    );
}

#[test]
fn incomplete_record_remains_pending() {
    let root = tempfile::tempdir().unwrap();
    let spool = root.path().join("spool");
    let reports = root.path().join("reports");
    ensure_private_dir(&spool).unwrap();
    let pending = spool.join("writing.rpcrash");
    fs::write(&pending, [1, 2, 3]).unwrap();
    assert!(ingest_pending(&spool, &reports).unwrap().is_empty());
    assert!(pending.exists());
}
