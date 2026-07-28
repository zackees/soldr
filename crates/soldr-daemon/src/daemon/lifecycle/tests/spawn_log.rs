//! soldr#1961 — the Windows daemon-spawn log redirect.
//!
//! A daemon that dies during startup on Windows used to leave no artifact at
//! all, while the Unix paths left `daemon-spawn.log` and `soldr logs`
//! advertised that file on every platform.

#[cfg(all(test, windows))]
mod windows_spawn_log_tests {
    use crate::daemon::lifecycle::*;
    use tempfile::TempDir;

    /// `GetHandleInformation` — reads back the inherit flag we set.
    fn handle_is_inheritable(file: &std::fs::File) -> bool {
        use std::os::windows::io::AsRawHandle;
        extern "system" {
            fn GetHandleInformation(
                hObject: std::os::windows::raw::HANDLE,
                lpdwFlags: *mut u32,
            ) -> i32;
        }
        const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
        let mut flags: u32 = 0;
        // SAFETY: `file` owns a live handle for the duration of the call.
        let ok = unsafe { GetHandleInformation(file.as_raw_handle(), &mut flags) };
        ok != 0 && (flags & HANDLE_FLAG_INHERIT) != 0
    }

    // The property the whole change rests on. `bInheritHandles: TRUE` only
    // passes handles already marked inheritable, and every handle named in a
    // PROC_THREAD_ATTRIBUTE_HANDLE_LIST must be inheritable or CreateProcessW
    // fails outright -- so if this flag is not set, the redirect silently
    // stops working and the artifact goes missing again.
    crate::timed_test!(the_spawn_log_handle_is_marked_inheritable, {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("daemon-spawn.log");
        let file = open_inheritable_spawn_log_at(&path).expect("open log");
        assert!(
            handle_is_inheritable(&file),
            "the child cannot receive a handle that is not marked inheritable"
        );
        assert!(path.is_file(), "the log file must exist after opening");
    });

    // Append, not truncate: successive spawns must not erase the evidence
    // from the crash that is being investigated.
    crate::timed_test!(the_spawn_log_appends_across_reopens, {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("daemon-spawn.log");
        std::fs::write(&path, b"earlier crash\n").expect("seed");
        drop(open_inheritable_spawn_log_at(&path).expect("reopen"));
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
        assert!(open_inheritable_spawn_log_at(&path).is_some());
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
            open_inheritable_spawn_log_at(&path).is_none(),
            "an unopenable log must yield None so the caller spawns unredirected"
        );
    });
}
