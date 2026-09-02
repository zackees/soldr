#[cfg(test)]
mod journal_hygiene_tests {
    //! soldr#2436 phase 2: rotation bound, successor-side unclean-shutdown
    //! detection, and the identity fields on start records.

    use crate::core::SoldrPaths;
    use crate::daemon::lifecycle::*;
    use tempfile::TempDir;

    fn temp_paths() -> (TempDir, SoldrPaths) {
        let tmp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        (tmp, paths)
    }

    fn journal_path(paths: &SoldrPaths) -> std::path::PathBuf {
        crate::cache_lib::daemon_lifecycle_log_path(paths)
    }

    fn journal_lines(paths: &SoldrPaths) -> Vec<String> {
        std::fs::read_to_string(journal_path(paths))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn rotation_truncates_over_threshold_and_keeps_newest() {
        let (_tmp, paths) = temp_paths();
        let path = journal_path(&paths);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content: String = (0..10_001).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        std::fs::write(&path, content).unwrap();

        rotate_lifecycle_journal(&paths);
        let lines = journal_lines(&paths);
        assert_eq!(lines.len(), 5_000);
        assert_eq!(lines.first().unwrap(), "{\"n\":5001}");
        assert_eq!(lines.last().unwrap(), "{\"n\":10000}");
    }

    #[test]
    fn rotation_leaves_small_journals_untouched() {
        let (_tmp, paths) = temp_paths();
        let path = journal_path(&paths);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"n\":1}\n{\"n\":2}\n").unwrap();
        rotate_lifecycle_journal(&paths);
        assert_eq!(journal_lines(&paths).len(), 2);
    }

    /// A pid that is definitely dead: spawn a trivial child through the
    /// sanctioned running-process boundary and reap it. (Synthetic
    /// sentinels like u32::MAX are unreliable — Unix pids are i32-backed
    /// and the platform probe may wrap them.)
    fn reaped_child_pid() -> u32 {
        let windows =
            crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows;
        let mut command = std::process::Command::new(if windows { "cmd" } else { "true" });
        if windows {
            command.args(["/c", "exit"]);
        }
        let stdio = running_process::SpawnStdio {
            stdin: running_process::StdioSource::Null,
            stdout: running_process::StdioSource::Null,
            stderr: running_process::StdioSource::Null,
            drain_timeout: None,
            show_console: false,
        };
        let mut child = running_process::spawn(&mut command, stdio).expect("spawn trivial child");
        let pid = child.id();
        child.wait().expect("reap child");
        pid
    }

    #[test]
    fn dead_pid_without_exit_record_is_reported_by_the_successor() {
        let (_tmp, paths) = temp_paths();
        let dead_pid = reaped_child_pid();
        let daemon_dir = crate::cache_lib::soldr_daemon_dir(&paths);
        std::fs::create_dir_all(&daemon_dir).unwrap();
        // Legacy two-line shape: pid then executable path.
        std::fs::write(
            daemon_dir.join("daemon.pid"),
            format!("{dead_pid}\n/tmp/soldr-daemon\n"),
        )
        .unwrap();

        detect_unclean_shutdown(&paths);
        let lines = journal_lines(&paths);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(record["event"], "unclean-shutdown-detected");
        assert_eq!(record["target_pid"], dead_pid);
        assert!(record["soldr_version"].is_string(), "{record}");
    }

    #[test]
    fn dead_pid_with_exit_record_is_not_reported() {
        let (_tmp, paths) = temp_paths();
        let dead_pid = reaped_child_pid();
        let daemon_dir = crate::cache_lib::soldr_daemon_dir(&paths);
        std::fs::create_dir_all(&daemon_dir).unwrap();
        std::fs::write(
            daemon_dir.join("daemon.pid"),
            format!("{dead_pid}\n/tmp/soldr-daemon\n"),
        )
        .unwrap();
        let path = journal_path(&paths);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("{{\"ts_ms\":1,\"pid\":{dead_pid},\"event\":\"died-shutdown\"}}\n"),
        )
        .unwrap();

        detect_unclean_shutdown(&paths);
        let lines = journal_lines(&paths);
        assert_eq!(lines.len(), 1, "no new record expected: {lines:?}");
    }

    /// soldr#3059: `died-signal-fast` is the fast-SIGTERM-path's exit
    /// record (`fast_exit_on_signal` in `server_runtime.rs`). It carries
    /// the same `died-` prefix as `died-idle` / `died-shutdown`, so this
    /// asserts `detect_unclean_shutdown` -- the one place in the codebase
    /// that actually *reads* the lifecycle journal to decide "clean or
    /// not" -- already recognizes it as deliberate without any change to
    /// its matching logic. This is the "reader can tell a marked
    /// end-of-stream from a truncated file" property, exercised against
    /// the real reader rather than a reimplementation of its matching
    /// rule.
    #[test]
    fn dead_pid_with_died_signal_fast_record_is_not_reported() {
        let (_tmp, paths) = temp_paths();
        let dead_pid = reaped_child_pid();
        let daemon_dir = crate::cache_lib::soldr_daemon_dir(&paths);
        std::fs::create_dir_all(&daemon_dir).unwrap();
        std::fs::write(
            daemon_dir.join("daemon.pid"),
            format!("{dead_pid}\n/tmp/soldr-daemon\n"),
        )
        .unwrap();
        let path = journal_path(&paths);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Same shape `append_lifecycle_event(paths, "died-signal-fast")`
        // produces for `LifecycleDetails::default()` (every optional field
        // skipped) -- see `dead_pid_with_exit_record_is_not_reported`
        // above for the same convention with `died-shutdown`. The daemon
        // that wrote this line is, by construction, the dead one: a live
        // test process cannot make the production writer stamp a pid other
        // than its own, which is exactly what makes this a *marked*
        // end-of-stream rather than the live process's actual exit.
        std::fs::write(
            &path,
            format!("{{\"ts_ms\":1,\"pid\":{dead_pid},\"event\":\"died-signal-fast\"}}\n"),
        )
        .unwrap();

        detect_unclean_shutdown(&paths);
        let lines = journal_lines(&paths);
        assert_eq!(
            lines.len(),
            1,
            "a died-signal-fast record must read as a deliberate exit, not an \
             unclean one: {lines:?}"
        );
    }

    /// The other half of the same property: a journal whose only record
    /// for `dead_pid` is torn mid-write (the shape a hard kill during the
    /// marker append would leave -- no closing brace, no `event` field at
    /// all) must still be reported unclean. Together with
    /// [`dead_pid_with_died_signal_fast_record_is_not_reported`] this is
    /// the RED/GREEN pair for "a reader must be able to tell this stream
    /// ended deliberately here from this file was truncated mid-write":
    /// a clean marker reads as deliberate, a torn one reads as unclean,
    /// through the one real reader in the codebase rather than a test-side
    /// reimplementation of its rule.
    #[test]
    fn dead_pid_with_truncated_trailing_record_is_reported() {
        let (_tmp, paths) = temp_paths();
        let dead_pid = reaped_child_pid();
        let daemon_dir = crate::cache_lib::soldr_daemon_dir(&paths);
        std::fs::create_dir_all(&daemon_dir).unwrap();
        std::fs::write(
            daemon_dir.join("daemon.pid"),
            format!("{dead_pid}\n/tmp/soldr-daemon\n"),
        )
        .unwrap();
        let path = journal_path(&paths);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Cut off before the `event` key is even written -- the honest
        // shape of "the process died mid-`write()`", not merely a missing
        // closing brace. `serde_json` would reject this line outright, and
        // it must not be mistaken for a `died-*` record either.
        std::fs::write(&path, format!("{{\"ts_ms\":1738183000,\"pid\":{dead_pid},")).unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).unwrap())
                .is_err(),
            "the fixture must actually be invalid JSON, or this test proves nothing"
        );

        detect_unclean_shutdown(&paths);
        // Deliberately not asserting on `journal_lines(&paths).len()`: the
        // fixture above ends without a trailing newline (the realistic
        // shape of a write torn between `writeln!`'s two separate
        // `write()` calls -- content, then the newline -- see
        // `fast_exit_on_signal`'s doc comment for the same non-atomicity
        // in the marker write this mirrors), so `detect_unclean_shutdown`'s
        // own append lands glued onto the same line rather than a clean
        // second one. That is a pre-existing quirk of appending onto an
        // unterminated file, not something this test is about. What
        // matters is that the torn fragment was not mistaken for a
        // `died-*` record and the unclean-shutdown record was written at
        // all.
        let raw = std::fs::read_to_string(journal_path(&paths)).unwrap();
        assert!(
            raw.contains("\"event\":\"unclean-shutdown-detected\""),
            "a torn trailing record must not be mistaken for a died-* marker: {raw:?}"
        );
        assert!(
            raw.contains(&format!("\"target_pid\":{dead_pid}")),
            "the unclean-shutdown record must name the dead pid: {raw:?}"
        );
    }

    #[test]
    fn recording_daemon_identity_carries_versions_and_exe() {
        let identity = LifecycleDetails::recording_daemon_identity();
        assert_eq!(
            identity.soldr_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert!(identity.zccache_version.is_some());
        assert!(identity.requester_exe.is_some());
        assert_eq!(identity.requester_pid, Some(std::process::id()));
    }
}
