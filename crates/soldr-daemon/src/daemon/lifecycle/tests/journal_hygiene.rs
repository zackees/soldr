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

    /// A pid that is definitely dead: spawn a trivial child and reap it.
    /// (Synthetic sentinels like u32::MAX are unreliable — Unix pids are
    /// i32-backed and the platform probe may wrap them.)
    fn reaped_child_pid() -> u32 {
        let windows =
            crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows;
        let mut command = std::process::Command::new(if windows { "cmd" } else { "true" });
        if windows {
            command.args(["/c", "exit"]);
        }
        let mut child = command.spawn().expect("spawn trivial child");
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
