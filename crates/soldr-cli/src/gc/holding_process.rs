//! Which running process is holding a `target/` open (soldr#2199).
//!
//! A mapped executable image cannot be unlinked while a process is running
//! it. That is the one condition measured to produce the `Access is denied`
//! (os error 5) on a leaf that cascades into `The directory is not empty`
//! (os error 145) on the parent — the error `gc target --purge` reported with
//! no path and no reason. Read-only attributes do not do it, and neither do
//! open file handles; both were measured and ruled out on the reporting
//! platform.
//!
//! So when a purge fails, the actionable question is not *what* survived but
//! *who is holding it*, and that has a concrete answer: the process list.
//!
//! Read-only and best-effort. This runs on a path that has already failed, so
//! every step degrades to "found nothing" rather than erroring — a diagnostic
//! that can itself fail is worse than no diagnostic.

use std::path::Path;

/// A process whose executable image lives under the surveyed directory.
/// The platform crate owns the enumeration (a Windows Toolhelp snapshot);
/// on hosts without a process-image walker the list is simply empty.
pub(super) use crate::platform::process::inspect::ProcessHolder as HoldingProcess;

/// Processes whose executable image lives under `dir`.
///
/// Only the executable is checked, not loaded DLLs. A DLL mapped from the
/// tree blocks a delete exactly the same way, but finding one requires
/// enumerating handles rather than processes, which is a different and far
/// heavier operation. Callers should treat an empty result as "no *process*
/// is obviously holding it", not as proof that nothing is.
///
/// The failure this diagnoses is Windows-specific: elsewhere an unlink
/// succeeds against a running image, so there is nothing to report.
pub(super) fn holders_under(dir: &Path) -> Vec<HoldingProcess> {
    crate::platform::process::inspect::holders_under(dir)
}

/// One clause for the failure log, or `None` when nothing was found.
pub(super) fn summarize(holders: &[HoldingProcess]) -> Option<String> {
    let first = holders.first()?;
    let name = first
        .exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| first.exe.display().to_string());

    Some(if holders.len() == 1 {
        format!(
            "pid {} ({name}) is running from this tree and must exit before it can be deleted",
            first.pid,
        )
    } else {
        format!(
            "{} processes are running from this tree, including pid {} ({name}); \
             they must exit before it can be deleted",
            holders.len(),
            first.pid,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    crate::timed_test!(no_holders_summarizes_to_nothing, {
        assert_eq!(summarize(&[]), None);
    });

    crate::timed_test!(a_single_holder_names_the_pid_and_the_binary, {
        let holders = vec![HoldingProcess {
            pid: 4321,
            exe: PathBuf::from("C:/repo/target/debug/deps/held.exe"),
        }];
        let line = summarize(&holders).expect("a holder must summarize");
        assert!(line.contains("pid 4321"), "{line}");
        assert!(line.contains("held.exe"), "{line}");
        assert!(line.contains("must exit"), "{line}");
    });

    crate::timed_test!(several_holders_report_the_count_and_one_example, {
        let holders = vec![
            HoldingProcess {
                pid: 1,
                exe: PathBuf::from("C:/repo/target/a.exe"),
            },
            HoldingProcess {
                pid: 2,
                exe: PathBuf::from("C:/repo/target/b.exe"),
            },
        ];
        let line = summarize(&holders).expect("holders must summarize");
        assert!(line.contains('2'), "{line}");
        assert!(line.contains("pid 1"), "{line}");
    });

    // An empty directory can have no holders, on any platform. This also
    // pins that the enumeration never panics or hangs on a real filesystem
    // path, which matters because it runs on an already-failed purge.
    crate::timed_test!(an_empty_tree_has_no_holders, {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(holders_under(dir.path()).is_empty());
    });

    crate::timed_test!(a_missing_directory_yields_no_holders, {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(holders_under(&dir.path().join("gone")).is_empty());
    });
}

#[cfg(all(test, windows))]
mod windows_live {
    use super::*;

    // End-to-end: does the enumeration find a *genuinely running* image?
    //
    // The sibling unit tests cover formatting and the empty cases, and would
    // all pass against an enumeration that never found anything. This is the
    // one that fails if the Win32 walk is wrong, so it is worth the spawn.
    //
    // Uses a copy of `cmd.exe` rather than a purpose-built fixture: the point
    // is that the *image* is mapped from inside the tree, and any real
    // executable demonstrates that.
    crate::timed_test!(a_process_running_from_the_tree_is_found_and_named, {
        let dir = tempfile::tempdir().expect("tempdir");
        let deps = dir.path().join("debug").join("deps");
        std::fs::create_dir_all(&deps).expect("mkdir");

        let held = deps.join("held.exe");
        let Ok(_) = std::fs::copy("C:/Windows/System32/cmd.exe", &held) else {
            // A locked-down image without cmd.exe is not a failure of this
            // code; skipping beats a red lane nobody can act on.
            eprintln!("skipping: cmd.exe could not be copied");
            return;
        };

        let mut child = std::process::Command::new(&held)
            .args(["/c", "ping -n 25 127.0.0.1 > nul"])
            .spawn()
            .expect("spawn the held binary");

        // Assert liveness before measuring: a child that already exited would
        // make this pass vacuously for the wrong reason.
        std::thread::sleep(std::time::Duration::from_millis(700));
        let alive = child.try_wait().expect("poll child").is_none();

        let found = holders_under(dir.path());
        let summary = summarize(&found);

        let _ = child.kill();
        let _ = child.wait();

        assert!(alive, "child exited early; the probe proved nothing");
        assert_eq!(
            found.len(),
            1,
            "expected exactly the held binary: {found:?}"
        );
        assert_eq!(found[0].pid, child.id());
        let line = summary.expect("a holder must summarize");
        assert!(line.contains("held.exe"), "{line}");
        assert!(line.contains("must exit"), "{line}");
    });
}
