#[cfg(test)]
mod spawn_lock_tests {
    use crate::daemon::lifecycle::*;
    use crate::core::SoldrPaths;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    #[test]
    fn expired_startup_deadline_returns_before_spawn_work() {
        let prepared = PreparedDaemonSpawn {
            executable: PathBuf::from("definitely-missing-soldr-daemon"),
            via_self: false,
            idle_timeout_secs: None,
        };
        let started = Instant::now();
        let error = spawn_prepared_daemon(
            &prepared,
            None,
            Some(Instant::now() - Duration::from_millis(1)),
        )
        .expect_err("expired startup deadline must fail before spawning");
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(matches!(
            error,
            LifecycleError::Io(ref io) if io.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    #[test]
    fn spawn_lock_is_exclusive_within_a_single_process() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        let first = acquire_spawn_lock(&paths).expect("first acquire");
        // Within the same process, a second non-blocking exclusive
        // lock attempt against the same file MUST be refused —
        // otherwise the spawn-herd cap (issue #474 acceptance
        // criterion) can't possibly hold.
        let second = acquire_spawn_lock(&paths);
        assert!(
            second.is_none(),
            "second acquire while first is held must return None",
        );
        drop(first);

        // After release the lock becomes available again — but not
        // necessarily on the very next instruction (#1873).
        //
        // `acquire_spawn_lock` uses `fs2::try_lock_exclusive`, which is
        // `flock(2)` on unix, and an `flock` belongs to the *open file
        // description*: it is released only once the LAST descriptor
        // referring to that description is closed. `Command::spawn` is
        // fork+exec, and between the fork and the exec the child owns a
        // copy of every descriptor the parent had — including this lock.
        // Rust opens files `O_CLOEXEC`, but that closes the descriptor at
        // *exec*, so a concurrently-forked child keeps the lock alive for
        // the width of its fork→exec window.
        //
        // Several sibling tests in this binary fork while this one runs
        // (`/bin/sh` in `exited_unreaped_child_is_not_alive` and
        // `via_self_daemon_forces_main_cli_argv0`, `/bin/ps`, and
        // `subprocess_probe_root_owner`, which re-execs the whole test
        // binary and so has the widest window). Under CI load that
        // overlapped the `drop`/re-acquire pair here often enough to fail
        // `main` on roughly half its runs.
        //
        // The invariant worth asserting is that the release is not
        // permanent, so poll for it. The exclusivity assertion above —
        // the actual subject of this test — stays immediate.
        let deadline = Instant::now() + Duration::from_secs(10);
        let third = loop {
            if let Some(lock) = acquire_spawn_lock(&paths) {
                break lock;
            }
            assert!(
                Instant::now() < deadline,
                "lock never became available after release",
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(third);
    }

    #[test]
    fn displacement_enabled_by_default_and_off_via_env() {
        // Default (unset) → enabled. The explicit off-values disable it.
        // Uses a process-global env var; keep this the only test that
        // touches SOLDR_DAEMON_DISPLACE so it can't race a sibling.
        let prior = std::env::var_os(SOLDR_DAEMON_DISPLACE_ENV);
        std::env::remove_var(SOLDR_DAEMON_DISPLACE_ENV);
        assert!(displacement_enabled(), "unset must be enabled");
        for off in ["off", "0", "false", "no", "OFF"] {
            std::env::set_var(SOLDR_DAEMON_DISPLACE_ENV, off);
            assert!(!displacement_enabled(), "{off} must disable");
        }
        std::env::set_var(SOLDR_DAEMON_DISPLACE_ENV, "on");
        assert!(displacement_enabled(), "any other value stays enabled");
        match prior {
            Some(v) => std::env::set_var(SOLDR_DAEMON_DISPLACE_ENV, v),
            None => std::env::remove_var(SOLDR_DAEMON_DISPLACE_ENV),
        }
    }

    crate::timed_test!(preflight_requires_endpoint_status_to_match_recorded_pid, {
        // Regression for #1832 and the PID-reuse review finding: a live
        // same-stem PID plus a current claim is insufficient when no
        // daemon answers on the recorded endpoint.
        assert!(!preflight_identity_matches(Some(41), None, true));
        assert!(!preflight_identity_matches(Some(41), Some(42), true));
        assert!(!preflight_identity_matches(Some(41), Some(41), false));
        assert!(preflight_identity_matches(Some(41), Some(41), true));
    });

    crate::timed_test!(
        preflight_never_displaces_a_daemon_that_claims_this_version,
        {
            // Regression for #1865. Reaching `preflight_should_displace` already
            // means the status probe returned nothing — which is ambiguous. When
            // the PID-file process is alive, one of ours, and publishing this
            // exact version, that ambiguity must resolve to "busy", not "stale".
            //
            // Every occupancy signal is set here precisely because those are what
            // used to authorize the kill: the fix is that a current-version claim
            // outranks all of them.
            assert!(
                !preflight_should_displace(true, true, true, true),
                "a daemon claiming the current version must survive a failed status probe"
            );
            assert!(!preflight_should_displace(true, false, false, false));
        }
    );

    crate::timed_test!(
        preflight_still_displaces_a_daemon_without_a_current_claim,
        {
            // The other half of #1865: the fix must not turn preflight into a
            // no-op. Absent a current-version claim, any single occupancy signal
            // still authorizes displacement — that is the #1495 behavior.
            assert!(
                preflight_should_displace(false, true, false, false),
                "a stale daemon holding the endpoint must still be displaced"
            );
            assert!(preflight_should_displace(false, false, true, false));
            assert!(preflight_should_displace(false, false, false, true));
            // Nothing present at all → nothing to displace.
            assert!(!preflight_should_displace(false, false, false, false));
        }
    );

    #[test]
    fn current_version_claim_matches_only_for_this_build() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        // No claim → version-unknown → not current.
        assert!(!current_version_claim_matches(&paths));

        // This build's own claim → current.
        crate::daemon::broker_discovery::write_root_version_claim(&paths).expect("write claim");
        assert!(current_version_claim_matches(&paths));

        // A stale writer's claim → not current (the mismatch that drives
        // displacement).
        use running_process::broker::protocol_v2::{write_to_root_v2, CacheManifestBuilder};
        let stale = CacheManifestBuilder::new(
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME,
            "0.0.0-stale",
        )
        .build();
        write_to_root_v2(&paths.root, &stale).expect("write stale");
        assert!(!current_version_claim_matches(&paths));
    }

    #[test]
    fn stale_daemon_occupancy_ignores_dead_pid() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        std::fs::create_dir_all(soldr_daemon_dir(&paths)).expect("daemon dir");
        // A large positive PID that is almost certainly not a running
        // process. (Not `u32::MAX`, which casts to the `-1` "all
        // processes" wildcard on Unix and would spuriously look alive.)
        std::fs::write(
            daemon_pid_path(&paths),
            format!("{}\nsoldr-daemon\n", i32::MAX as u32),
        )
        .expect("pid file");
        assert!(stale_daemon_occupies_endpoint(&paths).is_none());
        // Displacing a non-occupied endpoint is a successful no-op. Stale
        // shared artifacts are reclaimed by startup, not retirement.
        assert!(displace_stale_daemon(&paths));
    }

    #[test]
    fn direct_pid_file_live_accepts_expected_process_stem() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        std::fs::create_dir_all(soldr_daemon_dir(&paths)).expect("daemon dir");
        let current_exe = std::env::current_exe().expect("current exe");
        let current_stem = current_exe
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("current exe stem")
            .to_string();
        std::fs::write(
            daemon_pid_path(&paths),
            format!("{}\n{}\n", std::process::id(), current_exe.display()),
        )
        .expect("pid file");

        assert_eq!(
            direct_pid_file_live_for_stem(&paths, &current_stem),
            Some(std::process::id())
        );
    }

    #[cfg(unix)]
    crate::timed_test!(uninspectable_process_image_fails_closed, {
        assert!(
            !process_image_stem_matches(None, "soldr-daemon"),
            "an uninspectable process image must never be trusted"
        );
    });

    #[cfg(unix)]
    crate::timed_test!(unrelated_live_pid_is_not_displaced, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        std::fs::create_dir_all(soldr_daemon_dir(&paths)).expect("daemon dir");
        let current_exe = std::env::current_exe().expect("current exe");
        std::fs::write(
            daemon_pid_path(&paths),
            format!("{}\n{}\n", std::process::id(), current_exe.display()),
        )
        .expect("pid file");

        assert!(stale_daemon_occupies_endpoint(&paths).is_none());
        assert!(
            !displace_stale_daemon(&paths),
            "an unverified live PID without an IPC acknowledgement must fail closed"
        );
        assert!(pid_is_alive(std::process::id()));
    });

    crate::timed_test!(shutdown_wait_tracks_the_acknowledged_generation, {
        use crate::daemon::protocol::ShutdownAck;

        let responder = ShutdownAck {
            pid: 42,
            generation: 100,
        };
        assert_eq!(
            classify_shutdown_observation(responder, false, None),
            Some(ShutdownWaitOutcome::Exited)
        );
        assert_eq!(
            classify_shutdown_observation(responder, true, Some((42, 100))),
            None,
            "the acknowledged generation is still flushing"
        );
        assert_eq!(
            classify_shutdown_observation(responder, true, Some((42, 101))),
            Some(ShutdownWaitOutcome::Replaced),
            "PID reuse by a new daemon must not be mistaken for the old responder"
        );
        assert_eq!(
            classify_shutdown_observation(responder, true, Some((43, 100))),
            Some(ShutdownWaitOutcome::Replaced)
        );
    });

    #[test]
    fn spawn_lock_serializes_concurrent_threads() {
        let temp = TempDir::new().expect("tempdir");
        let paths = Arc::new(SoldrPaths::with_root(temp.path().to_path_buf()));
        const THREADS: usize = 16;
        let barrier = Arc::new(Barrier::new(THREADS));
        let success_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let paths = paths.clone();
            let barrier = barrier.clone();
            let counter = success_count.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                // Race-start: try to acquire. Holders hold the lock
                // briefly to simulate the relocate+spawn work the
                // real call would do.
                if let Some(guard) = acquire_spawn_lock(&paths) {
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    drop(guard);
                }
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }
        // Each successful acquire holds the lock for ~10ms; we expect
        // at MOST a handful to land sequentially in the few hundred
        // ms the test takes, but cap at THREADS - 1 because if all
        // threads acquired the lock it would defeat the purpose.
        let count = success_count.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            count >= 1,
            "at least one thread must acquire the lock; got {count}",
        );
        assert!(
            count < THREADS,
            "lock must serialize — fewer than {THREADS} acquires expected (the spawn-herd cap from #474); got {count}",
        );
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn subprocess_probe_root_owner() {
        let root = std::env::var_os("SOLDR_TEST_ROOT_OWNER_ROOT").expect("test root");
        let expected = std::env::var("SOLDR_TEST_ROOT_OWNER_EXPECT").expect("expectation");
        let paths = SoldrPaths::with_root(PathBuf::from(root));
        let acquired = RootOwnershipGuard::try_acquire(&paths)
            .expect("root ownership probe")
            .is_some();
        assert_eq!(acquired, expected == "acquired");
    }

    crate::timed_test!(root_ownership_is_version_blind_across_processes, {
        let temp = TempDir::new().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let run_probe = |expected: &str| {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "daemon::lifecycle::spawn_lock_tests::subprocess_probe_root_owner",
                    "--nocapture",
                ])
                .env("SOLDR_TEST_ROOT_OWNER_ROOT", &paths.root)
                .env("SOLDR_TEST_ROOT_OWNER_EXPECT", expected)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "subprocess root-owner probe failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };

        let owner = RootOwnershipGuard::try_acquire(&paths)
            .unwrap()
            .expect("parent owns exact root");
        run_probe("blocked");
        drop(owner);
        run_probe("acquired");
    });
}
