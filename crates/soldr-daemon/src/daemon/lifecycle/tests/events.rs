#[cfg(test)]
mod lifecycle_event_tests {
    use crate::core::SoldrPaths;
    use crate::daemon::lifecycle::*;
    use tempfile::TempDir;

    fn read_events(paths: &SoldrPaths) -> Vec<serde_json::Value> {
        let path = crate::cache_lib::daemon_lifecycle_log_path(paths);
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each line is a JSON object"))
            .collect()
    }

    // soldr#1808 requires extending this log "without breaking readers that
    // ignore unknown fields". The stricter property is the one worth pinning:
    // a record with no attribution must serialize exactly as it did before,
    // because `tests/cli_daemon_lifecycle.rs` matches the raw substring
    // `"event":"spawn"` rather than parsing. Absent fields must vanish, not
    // appear as nulls.
    crate::timed_test!(a_record_without_details_keeps_its_original_three_fields, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));

        append_lifecycle_event(&paths, "spawn");

        let events = read_events(&paths);
        assert_eq!(events.len(), 1);
        let obj = events[0].as_object().expect("object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["event", "pid", "ts_ms"],
            "an unattributed record must gain no keys; got {obj:?}"
        );
    });

    // The `event` field stays a stable identifier -- soldr#1808 asks for typed
    // details specifically so circumstances do not get concatenated into the
    // event name. Assert both halves: the name is untouched, and the detail
    // rides alongside it.
    crate::timed_test!(details_travel_beside_the_event_name_not_inside_it, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));

        append_lifecycle_event_with(
            &paths,
            "displace-kill-fallback",
            LifecycleDetails::forced(4321, LifecycleReason::ProtocolMismatch),
        );

        let events = read_events(&paths);
        assert_eq!(events[0]["event"], "displace-kill-fallback");
        assert_eq!(events[0]["target_pid"], 4321);
        assert_eq!(events[0]["reason"], "protocol-mismatch");
        assert_eq!(events[0]["outcome"], "forced");
    });

    // Both `displace-kill-fallback` sites knew the victim PID and dropped it
    // (soldr#1808: "Both `displace-kill-fallback` call sites omit the victim
    // PID and reason"). A record naming neither is indistinguishable from any
    // other kill, which is what made these unattributable in the first place.
    crate::timed_test!(a_forced_record_always_names_its_victim_and_reason, {
        let details = LifecycleDetails::forced(99, LifecycleReason::StartupDeadline);
        assert_eq!(details.target_pid, Some(99));
        assert_eq!(details.reason, Some(LifecycleReason::StartupDeadline));
        assert_eq!(details.outcome, Some(LifecycleOutcome::Forced));
    });

    // soldr#1808 WS3. Two `displace-kill-fallback` records are otherwise
    // identical whether a build's own preflight cleared a stale daemon or an
    // operator ran `soldr daemon stop` -- different causes, different
    // remedies. The source is what separates them.
    crate::timed_test!(the_source_distinguishes_otherwise_identical_records, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));

        let base = LifecycleDetails::forced(7, LifecycleReason::StaleVersion);
        append_lifecycle_event_with(
            &paths,
            "displace-kill-fallback",
            base.clone().from_source(Some(LifecycleSource::Preflight)),
        );
        append_lifecycle_event_with(
            &paths,
            "displace-kill-fallback",
            base.from_source(Some(LifecycleSource::Cli)),
        );

        let events = read_events(&paths);
        assert_eq!(events[0]["requester_source"], "preflight");
        assert_eq!(events[1]["requester_source"], "cli");
        assert_eq!(events[0]["event"], events[1]["event"]);
    });

    // An unattributed record must still omit the key, not emit null -- the
    // byte-identical guarantee for detail-free records covers this field too.
    crate::timed_test!(an_unattributed_record_omits_the_source, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        append_lifecycle_event_with(
            &paths,
            "displace-kill-fallback",
            LifecycleDetails::forced(7, LifecycleReason::StaleVersion),
        );
        let obj = read_events(&paths)[0].as_object().expect("object").clone();
        assert!(
            !obj.contains_key("requester_source"),
            "absent source must be omitted, not null; got {obj:?}"
        );
    });

    crate::timed_test!(graceful_shutdown_records_the_observed_peer_before_ack, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let peer = crate::daemon::ipc_peer::PeerIdentity {
            pid: Some(1234),
            exe: Some(r"C:\redacted\soldr.exe".into()),
            source: LifecycleSource::IpcPeer,
        };
        append_lifecycle_event_with(
            &paths,
            "shutdown-requested",
            LifecycleDetails::requested(LifecycleReason::ExplicitStop)
                .for_target_generation(4321, 9876)
                .with_peer(peer),
        );

        let event = &read_events(&paths)[0];
        assert_eq!(event["requester_pid"], 1234);
        assert_eq!(event["requester_exe"], r"C:\redacted\soldr.exe");
        assert_eq!(event["requester_source"], "ipc-peer");
        assert_eq!(event["target_pid"], 4321);
        assert_eq!(event["target_generation"], 9876);
        assert_eq!(event["reason"], "explicit-stop");
        assert_eq!(event["outcome"], "requested");
    });

    crate::timed_test!(unknown_transport_identity_never_invents_pid_or_exe, {
        let details = LifecycleDetails::requested(LifecycleReason::ExplicitStop)
            .with_peer(crate::daemon::ipc_peer::PeerIdentity::unknown());
        assert_eq!(details.requester_source, Some(LifecycleSource::Unknown));
        assert_eq!(details.requester_pid, None);
        assert_eq!(details.requester_exe, None);
    });

    crate::timed_test!(vanished_without_ack_is_a_distinct_observed_outcome, {
        let details =
            LifecycleDetails::vanished_without_ack(Some(55), LifecycleReason::ProtocolMismatch);
        assert_eq!(details.target_pid, Some(55));
        assert_eq!(details.outcome, Some(LifecycleOutcome::VanishedWithoutAck));
        assert_ne!(details.outcome, Some(LifecycleOutcome::Forced));
    });

    // A requested transition has no victim yet -- the field must be absent
    // rather than serialized as null, or readers cannot tell "not applicable"
    // from "we failed to record it".
    crate::timed_test!(
        a_requested_record_omits_the_target_rather_than_nulling_it,
        {
            let temp = TempDir::new().expect("tempdir");
            let paths = SoldrPaths::with_root(temp.path().join("root"));

            append_lifecycle_event_with(
                &paths,
                "displace-stale-requested",
                LifecycleDetails::requested(LifecycleReason::StaleVersion),
            );

            let obj = read_events(&paths)[0].as_object().expect("object").clone();
            assert!(
                !obj.contains_key("target_pid"),
                "absent detail must be omitted, not null; got {obj:?}"
            );
            assert_eq!(obj["reason"], "stale-version");
            assert_eq!(obj["outcome"], "requested");
        }
    );
}

#[cfg(test)]
mod root_ownership_diagnostic_tests {
    use crate::core::SoldrPaths;
    use crate::daemon::lifecycle::*;
    use tempfile::TempDir;

    // soldr#1987. The orphan case is the one that cost 28 hours: a live PID
    // whose image was deleted holds the lock forever, `soldr daemon stop`
    // cannot see it, and the only symptom is `compiler cache unavailable`.
    // The message has to name the PID and say the image is gone, or the user
    // has nothing to act on.
    crate::timed_test!(a_live_owner_with_a_deleted_image_is_named_as_recoverable, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        // This process is alive by construction; point the record at an image
        // that does not exist, which is exactly the orphan's shape.
        let me = std::process::id();
        let missing = temp.path().join("deleted-by-uv").join("soldr-daemon.exe");
        std::fs::create_dir_all(crate::cache_lib::soldr_daemon_dir(&paths)).expect("dir");
        std::fs::write(
            crate::cache_lib::daemon_pid_path(&paths),
            format!("{me}\n{}\n", missing.display()),
        )
        .expect("pid file");

        let msg = describe_root_ownership_conflict(&paths);
        assert!(msg.contains(&me.to_string()), "must name the PID: {msg}");
        assert!(
            msg.contains("no longer exists"),
            "must say the image is gone: {msg}"
        );
        assert!(
            msg.contains("soldr#1987"),
            "must point at the issue explaining why stop cannot reach it: {msg}"
        );
    });

    // A healthy owner is a normal single-instance conflict, not an orphan --
    // it must not be described as recoverable-by-killing.
    crate::timed_test!(a_live_owner_with_a_real_image_is_not_called_an_orphan, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let me = std::process::id();
        let real = std::env::current_exe().expect("current exe");
        std::fs::create_dir_all(crate::cache_lib::soldr_daemon_dir(&paths)).expect("dir");
        std::fs::write(
            crate::cache_lib::daemon_pid_path(&paths),
            format!("{me}\n{}\n", real.display()),
        )
        .expect("pid file");

        let msg = describe_root_ownership_conflict(&paths);
        assert!(msg.contains(&me.to_string()), "{msg}");
        assert!(
            !msg.contains("no longer exists"),
            "a present image must not be reported as missing: {msg}"
        );
    });

    // No PID file at all must still produce something better than silence.
    crate::timed_test!(a_missing_pid_file_says_so_rather_than_naming_nobody, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let msg = describe_root_ownership_conflict(&paths);
        assert!(msg.contains("no daemon PID file"), "{msg}");
    });
}

#[cfg(test)]
mod missing_image_detector_tests {
    use crate::daemon::lifecycle::*;

    // The safety property, and the reason for hysteresis at all: a daemon that
    // exits on one bad reading is worse than the orphan it prevents, because
    // it takes down healthy daemons for transient reasons.
    crate::timed_test!(a_single_missing_reading_never_triggers, {
        let mut d = MissingImageDetector::default();
        assert!(!d.observe(Some(false)));
        assert_eq!(d.strikes(), 1);
    });

    crate::timed_test!(it_triggers_exactly_once_on_the_confirming_strike, {
        let mut d = MissingImageDetector::default();
        let fired: Vec<bool> = (0..5).map(|_| d.observe(Some(false))).collect();
        assert_eq!(
            fired,
            vec![false, false, true, false, false],
            "must fire on strike {DAEMON_IMAGE_MISSING_STRIKES} and not again -- \
             a repeating trigger would re-request shutdown every tick"
        );
    });

    // One sighting proves the condition was not the permanent one.
    crate::timed_test!(any_sighting_resets_the_count, {
        let mut d = MissingImageDetector::default();
        d.observe(Some(false));
        d.observe(Some(false));
        assert_eq!(d.strikes(), 2, "primed one strike from firing");
        assert!(!d.observe(Some(true)));
        assert_eq!(d.strikes(), 0);
        assert!(!d.observe(Some(false)), "count must restart, not resume");
    });

    // Not knowing where our image is says nothing about whether it exists, so
    // it must not accumulate toward self-termination.
    crate::timed_test!(an_unknown_path_resets_rather_than_accumulating, {
        let mut d = MissingImageDetector::default();
        d.observe(Some(false));
        d.observe(Some(false));
        assert!(
            !d.observe(None),
            "unknown must not be the confirming strike"
        );
        assert_eq!(d.strikes(), 0);
    });

    // The live probe must agree with reality for the running test binary,
    // which exists by construction.
    crate::timed_test!(the_live_probe_sees_this_running_executable, {
        assert_eq!(daemon_image_present(), Some(true));
    });
}
