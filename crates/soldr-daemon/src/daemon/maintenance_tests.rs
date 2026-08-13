//! Unit tests for [`crate::daemon::maintenance`]: the pressure/full
//! maintenance loop, the cross-process sweep barrier, and the status
//! serialization round-trips. Lives in a sibling file referenced via
//! `#[path]` so `maintenance.rs` stays under the 1000-LOC ceiling.

use super::*;

/// Fixture directory shared between the sweep child process and the
/// probing parent — see [`sweep_never_holds_state_db_across_filesystem_work`].
pub(super) const FS_BARRIER_DIR_ENV: &str = "SOLDR_TEST_SWEEP_FS_BARRIER_DIR";
const FS_BARRIER_ROOT_ENV: &str = "SOLDR_TEST_SWEEP_FIXTURE_ROOT";
const SWEEP_CHILD_TEST: &str = "daemon::maintenance::tests::sweep_fs_phase_child_holds_the_barrier";

// The child half of the cross-process regression test.
//
// Inert unless the parent set [`FS_BARRIER_ROOT_ENV`], so a normal
// suite run treats it as a no-op case.
crate::timed_test!(sweep_fs_phase_child_holds_the_barrier, {
    let Some(root) = std::env::var_os(FS_BARRIER_ROOT_ENV).map(PathBuf::from) else {
        return;
    };
    let paths = SoldrPaths::with_root(root);
    // The parent's config, not `SoldrConfig::default()`: the default
    // allowlist is `~/dev`, which no tempdir fixture can satisfy, so
    // `evaluate_safety_guards` would reject the seeded row and phase 2
    // would iterate an empty vector (soldr#2225).
    let config = paths.load_config().expect("fixture config");
    // Parks in `fs_phase_barrier` for as long as the parent needs, then
    // runs the real delete loop.
    let outcome = sweep_workspace_targets(&paths, &config, MaintenanceKind::Full);
    // Handed back out-of-band so the parent can assert the sweep really
    // entered its delete loop rather than no-opping past it.
    if let Some(dir) = std::env::var_os(FS_BARRIER_DIR_ENV).map(PathBuf::from) {
        std::fs::write(
            dir.join("sweep-outcome"),
            serde_json::to_vec(&outcome).expect("serialize sweep outcome"),
        )
        .expect("write sweep outcome");
    }
});

// soldr#2224 acceptance: the daemon's maintenance sweep must not hold
// `state.redb` across its filesystem phase.
//
// Real processes, deliberately. The process-wide `state_db_open_lock`
// masks this bug in-thread — a second opener in the same process just
// *waits* on the mutex instead of failing — so an in-process test
// would pass against the broken code. Two processes see redb's actual
// file lock, which is what the front door hits in soldr#2223.
//
// It asserts the property rather than racing it: the child parks at
// the start of the handle-free phase and does not proceed until the
// parent has finished proving the database is reachable. Before this
// fix the child would still be holding the registry handle at that
// point and the parent's open would burn its whole 5 s budget and
// fail.
crate::timed_test!(
    sweep_never_holds_state_db_across_filesystem_work,
    Duration::from_secs(90),
    {
        let fixture = tempfile::tempdir().expect("tempdir");
        let root = fixture.path().join("soldr-root");
        let barrier = fixture.path().join("barrier");
        std::fs::create_dir_all(&barrier).expect("barrier dir");
        let paths = SoldrPaths::with_root(root.clone());
        let db_path = crate::cache_lib::data_db_path(&paths);

        // A registered target the sweep will actually *delete*, not
        // merely snapshot. Three things have to line up or the row is
        // filtered out before phase 2 and the delete loop runs zero
        // times (soldr#2225):
        //
        //  1. cargo markers, or `delete_candidate_dir` refuses (#1671);
        //  2. both age signals past the 30-day `FULL_STALE_AGE`, because
        //     `effective_age_seconds` takes the *younger* of the registry
        //     row and the directory mtime (soldr#2134) — a freshly
        //     created dir reads as age 0 no matter what the row says;
        //  3. an allowlist root containing the workspace, since the
        //     default `~/dev` cannot contain a tempdir.
        let workspace = fixture.path().join("repo");
        let target = workspace.join("target");
        std::fs::create_dir_all(&target).expect("target dir");
        std::fs::write(target.join("CACHEDIR.TAG"), b"Signature: 8a477f597d28d172")
            .expect("cachedir tag");
        // A populated tree, so phase 2 is a real recursive deletion
        // rather than a single unlink.
        for bucket in 0..6 {
            let dir = target.join("deps").join(format!("b{bucket}"));
            std::fs::create_dir_all(&dir).expect("deps bucket");
            for file in 0..200 {
                std::fs::write(dir.join(format!("o{file}.o")), vec![0u8; 256])
                    .expect("object file");
            }
        }

        std::fs::create_dir_all(&paths.root).expect("soldr root");
        let allowlist = workspace.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            &paths.config_file,
            format!("[auto_gc]\nenabled = true\n[gc]\nallowlist_roots = [\"{allowlist}\"]\n"),
        )
        .expect("fixture config");

        let stale = SystemTime::now() - Duration::from_secs(90 * 24 * 60 * 60);
        filetime::set_file_mtime(&target, filetime::FileTime::from_system_time(stale))
            .expect("age the target dir");
        {
            let registry = TargetRegistry::open(&db_path).expect("seed registry");
            registry
                .upsert_with_time(&target, unix_millis(stale) / 1_000)
                .expect("seed row");
        }

        // Configured with `std::process::Command`, executed through
        // running-process: this crate's process-creation boundary is
        // enforced by the `ban_raw_process_creation` dylint, tests
        // included.
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", SWEEP_CHILD_TEST, "--nocapture"])
            .env(FS_BARRIER_ROOT_ENV, &root)
            .env(FS_BARRIER_DIR_ENV, &barrier);
        let mut child = running_process::spawn(
            &mut command,
            running_process::SpawnStdio {
                stdin: running_process::StdioSource::Null,
                stdout: running_process::StdioSource::Parent,
                stderr: running_process::StdioSource::Parent,
                drain_timeout: Some(Duration::from_secs(5)),
                show_console: false,
            },
        )
        .expect("spawn sweep child");

        let sweeping = barrier.join("sweeping");
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        while !sweeping.exists() {
            assert!(
                child.try_wait().expect("poll sweep child").is_none(),
                "sweep child exited before reaching its filesystem phase"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "sweep child never reached its filesystem phase"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // Exactly what `persist_start_fallback_inner` does on the front
        // door when the daemon is unreachable: one handle, three ops.
        let started = std::time::Instant::now();
        let result = (|| -> Result<(), String> {
            let handle = db::open_handle(&db_path).map_err(|e| e.to_string())?;
            let existing = db::get_build_in(&handle, 4242).map_err(|e| e.to_string())?;
            assert!(existing.is_none(), "fixture starts with no session row");
            db::upsert_build_in(
                &handle,
                &crate::daemon::protocol::BuildRecord {
                    session_id: 4242,
                    repo_root: "/repo".into(),
                    started_at_ms: 1_000,
                    ended_at_ms: None,
                    exit_code: None,
                    total_wall_ms: None,
                    crate_count: 0,
                    slowest_crate_us: None,
                    slowest_crate_name: None,
                    cache_summary: None,
                    log_paths: None,
                    miss_reasons: Vec::new(),
                },
            )
            .map_err(|e| e.to_string())?;
            db::append_event_in(
                &handle,
                &db::Event {
                    ts_ms: 1_000,
                    session_id: Some(4242),
                    kind: db::EventKind::SessionStart,
                    crate_name: None,
                    duration_us: None,
                    target_dir: None,
                    exit_code: None,
                },
            )
            .map_err(|e| e.to_string())
        })();
        let elapsed = started.elapsed();

        std::fs::write(barrier.join("release"), b"").expect("release sweep child");
        let exit_code = child.wait().expect("wait for sweep child");

        result.unwrap_or_else(|error| {
            panic!(
                "the build-session fallback lost its record while the daemon was \
                 mid-maintenance-sweep: {error}. The sweep is holding state.redb \
                 across its filesystem phase again (soldr#2224)."
            )
        });
        assert!(
            elapsed < Duration::from_secs(2),
            "the fallback stalled {elapsed:?} waiting for the sweep's handle; it must \
             not wait at all (soldr#2224)"
        );
        assert_eq!(exit_code, 0, "sweep child failed");
        // The record really landed, not just "the open succeeded".
        let stored = db::get_build(&db_path, 4242).expect("read back");
        assert_eq!(stored.expect("record persisted").session_id, 4242);

        // ...and the sweep really deleted a populated tree while the
        // probe above was running. Without this the fixture could be
        // filtered out before phase 2 and the test would still pass,
        // proving only that no handle is held entering an empty loop.
        let outcome: ComponentOutcome = serde_json::from_slice(
            &std::fs::read(barrier.join("sweep-outcome")).expect("read sweep outcome"),
        )
        .expect("parse sweep outcome");
        assert_eq!(outcome.error, None, "sweep reported an error");
        assert_eq!(
            outcome.items_removed, 1,
            "the sweep must have entered its delete loop; the seeded target was \
             filtered out before phase 2 (soldr#2225)"
        );
        assert!(
            !target.exists(),
            "the stale target/ tree must be gone after the sweep"
        );
    }
);

crate::timed_test!(schedule_has_five_minute_pressure_and_daily_catchup, {
    let day = Duration::from_secs(24 * 60 * 60);
    let base = UNIX_EPOCH + Duration::from_secs(10 * day.as_secs());
    assert_eq!(due_kind(None, None, base), Some(MaintenanceKind::Full));
    assert_eq!(
        due_kind(Some(base), None, base),
        Some(MaintenanceKind::Pressure)
    );
    assert_eq!(
        due_kind(
            Some(base),
            Some(base),
            base + PRESSURE_INTERVAL - Duration::from_secs(1)
        ),
        None
    );
    assert_eq!(
        due_kind(Some(base), Some(base), base + PRESSURE_INTERVAL),
        Some(MaintenanceKind::Pressure)
    );
    assert_eq!(
        due_kind(Some(base), Some(base), base + day),
        Some(MaintenanceKind::Full)
    );
});

crate::timed_test!(
    failed_full_attempt_is_backed_off_without_claiming_success,
    {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let attempt = UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        record_last_full_attempt(&paths, attempt).unwrap();
        assert_eq!(read_last_full(&paths), None);
        assert_eq!(read_last_full_attempt(&paths), Some(attempt));
        assert_eq!(
            due_kind(
                read_last_full_attempt(&paths),
                Some(attempt),
                attempt + PRESSURE_INTERVAL,
            ),
            Some(MaintenanceKind::Pressure),
        );
        assert_eq!(
            due_kind(
                read_last_full_attempt(&paths),
                Some(attempt),
                attempt + FULL_INTERVAL,
            ),
            Some(MaintenanceKind::Full),
        );
    }
);

crate::timed_test!(
    maintenance_paths_are_distinct_for_prod_dev_custom_and_standalone,
    {
        let temp = tempfile::tempdir().unwrap();
        let prod = SoldrPaths::with_root(temp.path().join(".soldr"));
        let dev = SoldrPaths::with_root(temp.path().join(".soldr-dev"));
        let custom = SoldrPaths::with_root(temp.path().join("custom"));
        let standalone = temp.path().join(".zccache/sentinel");
        std::fs::create_dir_all(standalone.parent().unwrap()).unwrap();
        std::fs::write(&standalone, b"keep").unwrap();
        let paths = [status_path(&prod), status_path(&dev), status_path(&custom)];
        assert_ne!(paths[0], paths[1]);
        assert_ne!(paths[0], paths[2]);
        assert_ne!(paths[1], paths[2]);
        for path in paths {
            assert!(path.starts_with(temp.path()));
            assert!(!path.starts_with(temp.path().join(".zccache")));
        }
        assert_eq!(std::fs::read(&standalone).unwrap(), b"keep");
    }
);

crate::timed_test!(daemon_maintenance_never_installs_an_os_scheduler, {
    let source = include_str!("maintenance.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    for forbidden in ["schtasks", "systemctl", "launchctl", "Task Scheduler"] {
        assert!(
            !source.contains(forbidden),
            "daemon maintenance must not invoke/install {forbidden}"
        );
    }
});

crate::timed_test!(shutdown_waits_for_an_already_started_pass, {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        std::fs::create_dir_all(&paths.root).unwrap();
        let shutdown = Arc::new(ShutdownSignal::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let completed = paths.root.join("pass-completed");
        let task_paths = paths.clone();
        let task_shutdown = Arc::clone(&shutdown);
        let task_release = Arc::clone(&release);
        let task_completed = completed.clone();
        let handle = tokio::spawn(async move {
            let mut started_tx = Some(started_tx);
            run_loop_inner(
                &task_paths,
                task_shutdown,
                Duration::from_secs(3600),
                move |_, _| {
                    let started = started_tx.take();
                    let release = Arc::clone(&task_release);
                    let completed = task_completed.clone();
                    async move {
                        if let Some(started) = started {
                            let _ = started.send(());
                        }
                        release.acquire().await.unwrap().forget();
                        std::fs::write(completed, b"done").unwrap();
                        true
                    }
                },
            )
            .await;
        });
        started_rx.await.unwrap();
        shutdown.request();
        tokio::task::yield_now().await;
        assert!(!handle.is_finished(), "active maintenance was cancelled");
        release.add_permits(1);
        handle.await.unwrap();
        assert!(completed.is_file());
    });
});

crate::timed_test!(deferred_full_pass_remains_due_on_next_pressure_tick, {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        std::fs::create_dir_all(&paths.root).unwrap();
        let shutdown = Arc::new(ShutdownSignal::default());
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_in_pass = Arc::clone(&observed);
        let shutdown_in_pass = Arc::clone(&shutdown);
        let marker_paths = paths.clone();

        run_loop_inner(
            &paths,
            Arc::clone(&shutdown),
            Duration::from_millis(10),
            move |kind, _| {
                let mut observed = observed_in_pass.lock().unwrap();
                observed.push(kind);
                let pass_started = observed.len() > 1;
                if pass_started {
                    assert_eq!(read_last_full_attempt(&marker_paths), None);
                    shutdown_in_pass.request();
                }
                async move { pass_started }
            },
        )
        .await;

        assert_eq!(
            *observed.lock().unwrap(),
            vec![MaintenanceKind::Full, MaintenanceKind::Full]
        );
        assert!(read_last_full_attempt(&paths).is_some());
    });
});

crate::timed_test!(
    acquired_failed_full_pass_is_backed_off_without_success_marker,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let paths = SoldrPaths::with_root(temp.path().join("owned"));
            std::fs::create_dir_all(&paths.root).unwrap();
            let shutdown = Arc::new(ShutdownSignal::default());
            let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
            let observed_in_pass = Arc::clone(&observed);
            let shutdown_in_pass = Arc::clone(&shutdown);

            run_loop_inner(
                &paths,
                Arc::clone(&shutdown),
                Duration::from_millis(10),
                move |kind, _| {
                    let mut observed = observed_in_pass.lock().unwrap();
                    observed.push(kind);
                    if observed.len() > 1 {
                        shutdown_in_pass.request();
                    }
                    // The pass acquired the maintenance lease, even though its
                    // component status is modeled as failed/no success marker.
                    async { true }
                },
            )
            .await;

            assert_eq!(
                *observed.lock().unwrap(),
                vec![MaintenanceKind::Full, MaintenanceKind::Pressure]
            );
            assert!(read_last_full_attempt(&paths).is_some());
            assert_eq!(read_last_full(&paths), None);
        });
    }
);

crate::timed_test!(
    disabled_auto_gc_never_sweeps_registered_workspace_targets,
    {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let workspace = temp.path().join("workspace");
        let target = workspace.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("artifact"), b"keep").unwrap();
        std::fs::create_dir_all(&paths.root).unwrap();
        let allowlist = workspace.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            &paths.config_file,
            format!("[auto_gc]\nenabled = false\n[gc]\nallowlist_roots = [\"{allowlist}\"]\n"),
        )
        .unwrap();
        let db_path = crate::cache_lib::data_db_path(&paths);
        {
            let registry = TargetRegistry::open(&db_path).unwrap();
            registry.upsert_with_time(&target, 0).unwrap();
        }

        let outcomes = run_local_components(
            &paths,
            &db_path,
            MaintenanceKind::Full,
            SystemTime::now(),
            true,
        );
        assert_eq!(outcomes.workspace_targets, ComponentOutcome::default());
        assert!(target.join("artifact").is_file());
        let registry = TargetRegistry::open(&db_path).unwrap();
        assert_eq!(registry.list().unwrap().len(), 1);
    }
);

crate::timed_test!(invalid_config_does_not_disable_independent_collectors, {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    std::fs::create_dir_all(paths.root.join("trash-C/item")).unwrap();
    std::fs::write(paths.root.join("trash-C/item/stale"), b"delete").unwrap();
    std::fs::write(&paths.config_file, "[auto_gc\nenabled = true").unwrap();
    let db_path = crate::cache_lib::data_db_path(&paths);
    TargetRegistry::open(&db_path).unwrap();
    db::ensure_initialized(&db_path).unwrap();

    let outcomes = run_local_components(
        &paths,
        &db_path,
        MaintenanceKind::Full,
        SystemTime::now(),
        true,
    );
    assert!(outcomes
        .cook
        .error
        .as_deref()
        .unwrap()
        .contains("invalid_config"));
    assert!(outcomes
        .workspace_targets
        .error
        .as_deref()
        .unwrap()
        .contains("invalid_config"));
    assert_eq!(outcomes.trash.items_removed, 1);
    assert!(!paths.root.join("trash-C/item").exists());
    assert!(outcomes.history.error.is_none());
    assert!(outcomes.pep517_targets.error.is_none());
    assert!(outcomes.legacy_zccache.error.is_none());
});
