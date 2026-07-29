//! soldr#1961 — the daemon-spawn log redirect.
//!
//! A daemon that dies during startup on Windows used to leave no artifact at
//! all, while the Unix paths left `daemon-spawn.log` and `soldr logs`
//! advertised that file on every platform.

#[cfg(test)]
mod spawn_log_tests {
    use crate::daemon::lifecycle::*;
    use tempfile::TempDir;

    // Append, not truncate: successive spawns must not erase the evidence
    // from the crash that is being investigated.
    crate::timed_test!(the_spawn_log_appends_across_reopens, {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("daemon-spawn.log");
        std::fs::write(&path, b"earlier crash\n").expect("seed");
        drop(open_spawn_log_at(&path).expect("reopen"));
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            body.contains("earlier crash"),
            "reopening must not truncate prior output, got: {body:?}"
        );
    });

    // Missing parent directories are created rather than failing the spawn --
    // the log lives under the soldr root, which may not exist yet on a first
    // run.
    crate::timed_test!(a_missing_parent_directory_is_created, {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp
            .path()
            .join("nested")
            .join("deeper")
            .join("daemon-spawn.log");
        assert!(open_spawn_log_at(&path).is_some());
        assert!(path.is_file());
    });

    // An unopenable log must degrade to today's no-redirect spawn, never fail
    // it: losing the diagnostic is bad, losing the daemon is worse. A path
    // whose "parent" is an existing *file* cannot be created as a directory.
    crate::timed_test!(an_unopenable_log_degrades_instead_of_failing, {
        let tmp = TempDir::new().expect("tempdir");
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("write blocker");
        let path = blocker.join("daemon-spawn.log");
        assert!(
            open_spawn_log_at(&path).is_none(),
            "an unopenable log must yield None so the caller spawns unredirected"
        );
    });
}
