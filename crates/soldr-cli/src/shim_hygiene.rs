//! Detect an unmanaged `soldr` shadowing the installed `soldr.exe` (#1979).
//!
//! The Windows wheel installs exactly one entry point, `Scripts/soldr.exe`.
//! Older installs also left an *extensionless* `Scripts/soldr` behind, and
//! nothing owns it: `pip install --upgrade` and even
//! `pip install --force-reinstall` rewrite `soldr.exe` to a new inode and
//! never touch the sibling.
//!
//! That would be harmless except MSYS-derived shells (Git Bash, the one most
//! people run `soldr cargo …` from) resolve an extensionless file *ahead* of
//! `.exe` on `PATH`. So the stale sibling wins every bare `soldr`
//! invocation, and no upgrade can dislodge it.
//!
//! The failure is quiet and durable. On one machine the sibling sat four
//! releases behind for days: every command ran the old CLI, and because
//! `soldr` materializes its daemon from `current_exe()`, the daemon was
//! faithfully materialized *stale* to match. Nothing was corrupt --
//! materialization did exactly the right thing for the binary that was
//! actually running -- which is precisely why it was hard to see.
//!
//! Deliberately not a hot-path check: this reads the filesystem, and the
//! per-invocation budget is already under scrutiny (#1843). It runs from
//! `soldr doctor`, where paying for a `stat` is the entire point.

use std::path::{Path, PathBuf};

/// A running executable that shadows a managed `.exe` sibling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShimShadowing {
    /// The extensionless file that wins on `PATH`.
    pub running: PathBuf,
    /// The installer-managed `.exe` beside it.
    pub installed: PathBuf,
}

/// The managed `.exe` sibling that `current` would shadow, ignoring whether
/// it exists.
///
/// Pure path logic, split out so the rule is testable without a filesystem
/// and on every platform. Only an extensionless path can shadow, because
/// that is the whole mechanism: a file with an extension is not what a shell
/// picks up for bare `soldr`.
pub(crate) fn shadowed_sibling_path(current: &Path) -> Option<PathBuf> {
    if current.extension().is_some() {
        return None;
    }
    let name = current.file_name()?;
    let mut sibling = name.to_os_string();
    sibling.push(".exe");
    Some(current.with_file_name(sibling))
}

/// Report shadowing when `current` is extensionless and a *different* `.exe`
/// sibling exists.
///
/// "Different" is by length: identical length means the two are almost
/// certainly the same materialized binary (soldr hardlinks its own aliases,
/// so a matching pair is the healthy steady state and must not be reported).
/// A content hash would be more precise but this runs against a 33 MB binary
/// and the length check separates the states we care about.
pub(crate) fn detect_shadowing_at(current: &Path) -> Option<ShimShadowing> {
    let installed = shadowed_sibling_path(current)?;
    let running_len = std::fs::metadata(current).ok()?.len();
    let installed_len = std::fs::metadata(&installed).ok()?.len();
    if running_len == installed_len {
        return None;
    }
    Some(ShimShadowing {
        running: current.to_path_buf(),
        installed,
    })
}

/// [`detect_shadowing_at`] against the running executable.
pub(crate) fn detect_shadowing() -> Option<ShimShadowing> {
    detect_shadowing_at(&std::env::current_exe().ok()?)
}

/// Operator-facing explanation. Separate from the printer so tests can
/// assert the wording without capturing stdout.
pub(crate) fn shadowing_report(found: &ShimShadowing) -> String {
    format!(
        "  status:    SHADOWED -- the soldr you are running is not the installed one\n\
         \x20 running:   {}\n\
         \x20 installed: {}\n\
         \x20 why:       MSYS shells (Git Bash) resolve an extensionless file ahead of .exe,\n\
         \x20            so this sibling wins every bare `soldr` command. No installer owns\n\
         \x20            it, so upgrading soldr cannot replace or remove it.\n\
         \x20 impact:    commands run the shadowing binary, and the daemon is materialized\n\
         \x20            from it, so both stay behind the installed version indefinitely.\n\
         \x20 fix:       remove the shadowing file, then re-run any soldr command:\n\
         \x20              rm '{}'",
        found.running.display(),
        found.installed.display(),
        found.running.display()
    )
}

/// Outcome of an explicit [`remove_shadowing_at`] request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RemovalOutcome {
    /// Nothing was shadowing; nothing to do.
    NothingToDo,
    /// The shadowing file was deleted outright.
    Removed(PathBuf),
    /// The shadowing file could not be deleted because it is the running
    /// image, so it was renamed out of the way instead. That is the fix:
    /// once it no longer has the bare `soldr` name it cannot win on PATH.
    Renamed { from: PathBuf, to: PathBuf },
    /// Removal was attempted and failed, with the reason.
    Failed { path: PathBuf, reason: String },
}

/// Remove the shadowing file, if there is one.
///
/// soldr#1979. #1983 shipped detection only, and was explicit about why it
/// stopped there: "removing a binary from `PATH` behind the user's back is
/// the kind of action that must not be a side effect of a diagnostic."
///
/// That objection is to *implicitness*, not to removal — so this is reachable
/// only from `soldr doctor --remove-shadowing-shim`, which a user types. It
/// never runs as part of a plain `doctor`.
///
/// **Deleting outright usually fails, by construction.** Windows holds an
/// exclusive lock on a running image, and the shadowing file is precisely
/// what is running — that is the whole mechanism: it wins the bare `soldr`
/// lookup, so it is the binary executing this code. A plain
/// `std::fs::remove_file` therefore returns `Access is denied. (os error 5)`.
/// Verified end to end; the first version of this function did exactly that
/// and repaired nothing.
///
/// Renaming a running image *is* permitted, and it is sufficient: the file
/// only shadows because it is named `soldr`, so moving it aside fixes the
/// machine immediately even though the bytes are still on disk. This is the
/// same technique Windows self-updaters use.
///
/// So: try the clean delete first — it succeeds when this is invoked *via*
/// the installed `.exe`, where the orphan is not the running image — and fall
/// back to renaming. A failure at both is reported rather than swallowed,
/// because a silent no-op would leave the user believing they had fixed a
/// machine that is still broken.
pub(crate) fn remove_shadowing_at(current: &Path) -> RemovalOutcome {
    let Some(found) = detect_shadowing_at(current) else {
        return RemovalOutcome::NothingToDo;
    };
    match std::fs::remove_file(&found.running) {
        Ok(()) => return RemovalOutcome::Removed(found.running),
        Err(err) if err.kind() != std::io::ErrorKind::PermissionDenied => {
            return RemovalOutcome::Failed {
                path: found.running,
                reason: err.to_string(),
            };
        }
        Err(_) => {}
    }
    let aside = disused_name_for(&found.running);
    match std::fs::rename(&found.running, &aside) {
        Ok(()) => RemovalOutcome::Renamed {
            from: found.running,
            to: aside,
        },
        Err(err) => RemovalOutcome::Failed {
            path: found.running,
            reason: format!("delete denied and rename failed: {err}"),
        },
    }
}

/// A sibling name that cannot shadow: it carries an extension, so no shell
/// picks it for bare `soldr`.
fn disused_name_for(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".shadowed-disused");
    path.with_file_name(name)
}

/// Operator-facing text for a [`RemovalOutcome`]. Split from the printer so
/// the wording is assertable without capturing stdout.
pub(crate) fn removal_report(outcome: &RemovalOutcome) -> String {
    match outcome {
        RemovalOutcome::NothingToDo => {
            "  status:    ok (nothing shadowing the installed soldr; nothing removed)".to_string()
        }
        RemovalOutcome::Renamed { from, to } => format!(
            "  status:    RENAMED ASIDE (it is the running image, so it cannot be deleted)
               was:       {}
               now:       {}
               effect:    it no longer has the bare `soldr` name, so it can no longer win
                          on an MSYS PATH -- the machine is fixed now
               next:      re-run any soldr command; delete the renamed file at leisure",
            from.display(),
            to.display()
        ),
        RemovalOutcome::Removed(path) => format!(
            "  status:    REMOVED
               removed:   {}
               next:      re-run any soldr command; it will resolve to the installed .exe",
            path.display()
        ),
        RemovalOutcome::Failed { path, reason } => format!(
            "  status:    FAILED to remove
               path:      {}
               reason:    {}
               next:      close other soldr processes (`soldr daemon stop`) and retry, or
                          remove it by hand",
            path.display(),
            reason
        ),
    }
}

/// Print the `shim hygiene:` section for an explicit removal request.
pub(crate) fn print_shim_removal_section() {
    println!();
    println!("shim hygiene:");
    let outcome = match std::env::current_exe() {
        Ok(exe) => remove_shadowing_at(&exe),
        Err(err) => RemovalOutcome::Failed {
            path: PathBuf::from("<current_exe unavailable>"),
            reason: err.to_string(),
        },
    };
    println!("{}", removal_report(&outcome));
}

/// Print the `shim hygiene:` doctor section.
pub(crate) fn print_shim_hygiene_section() {
    println!();
    println!("shim hygiene:");
    match detect_shadowing() {
        Some(found) => println!("{}", shadowing_report(&found)),
        None => println!("  status:    ok (no unmanaged binary shadowing the installed soldr)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // soldr#1979 remediation. The detection half shipped in #1983; these
    // cover the removal it deliberately left out.
    crate::timed_test!(
        removal_deletes_the_shadowing_file_and_spares_the_installed_one,
        {
            let tmp = tempfile::tempdir().expect("tmpdir");
            let running = tmp.path().join("soldr");
            let installed = tmp.path().join("soldr.exe");
            // Different lengths: that is what `detect_shadowing_at` keys on.
            std::fs::write(&running, b"stale").expect("write running");
            std::fs::write(&installed, b"installed binary, longer").expect("write installed");

            let outcome = remove_shadowing_at(&running);
            assert_eq!(outcome, RemovalOutcome::Removed(running.clone()));
            assert!(!running.exists(), "the shadowing file must be gone");
            assert!(
            installed.exists(),
            "the installed .exe must never be touched -- removing it would              uninstall soldr instead of repairing it"
        );
        }
    );

    // Equal lengths are the healthy hardlinked steady state. Removing there
    // would delete a perfectly good alias.
    crate::timed_test!(removal_leaves_a_matching_pair_alone, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let running = tmp.path().join("soldr");
        let installed = tmp.path().join("soldr.exe");
        std::fs::write(&running, b"same").expect("w1");
        std::fs::write(&installed, b"same").expect("w2");

        assert_eq!(remove_shadowing_at(&running), RemovalOutcome::NothingToDo);
        assert!(running.exists(), "a healthy alias must survive");
        assert!(installed.exists());
    });

    // No sibling at all: the common case on a machine that was never
    // affected. Must not report a removal it did not perform.
    crate::timed_test!(removal_is_a_no_op_without_a_managed_sibling, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let running = tmp.path().join("soldr");
        std::fs::write(&running, b"lonely").expect("write");

        assert_eq!(remove_shadowing_at(&running), RemovalOutcome::NothingToDo);
        assert!(running.exists());
    });

    // A failure must say so. A silent no-op would leave the user believing
    // they had repaired a machine that is still broken.
    crate::timed_test!(a_failed_removal_is_reported_with_the_reason, {
        let outcome = RemovalOutcome::Failed {
            path: PathBuf::from("C:/tools/Scripts/soldr"),
            reason: "Access is denied. (os error 5)".to_string(),
        };
        let text = removal_report(&outcome);
        assert!(text.contains("FAILED"), "got: {text}");
        assert!(
            text.contains("os error 5"),
            "must carry the cause, got: {text}"
        );
        assert!(
            text.contains("daemon stop"),
            "must say what to try next, got: {text}"
        );
    });

    // The rename fallback is the path that actually runs in the real world:
    // the orphan is what wins on PATH, so it is the running image, and
    // Windows refuses to delete a running image. The first version of this
    // feature only tried `remove_file` and repaired nothing -- verified end to
    // end with `Access is denied. (os error 5)`.
    crate::timed_test!(the_disused_name_can_no_longer_shadow, {
        let aside = disused_name_for(Path::new("C:/tools/Scripts/soldr"));
        assert_eq!(
            aside,
            PathBuf::from("C:/tools/Scripts/soldr.shadowed-disused")
        );
        // The whole point: a name with an extension is not what a shell picks
        // for bare `soldr`, so the renamed file cannot shadow anything.
        assert_eq!(shadowed_sibling_path(&aside), None);
    });

    crate::timed_test!(a_rename_report_says_the_machine_is_already_fixed, {
        let text = removal_report(&RemovalOutcome::Renamed {
            from: PathBuf::from("C:/s/soldr"),
            to: PathBuf::from("C:/s/soldr.shadowed-disused"),
        });
        assert!(text.contains("RENAMED ASIDE"), "got: {text}");
        assert!(
            text.contains("fixed now"),
            "a rename is the fix, not a partial step -- the user must not be              left thinking more is required, got: {text}"
        );
    });

    crate::timed_test!(a_successful_removal_says_what_to_do_next, {
        let text = removal_report(&RemovalOutcome::Removed(PathBuf::from("C:/s/soldr")));
        assert!(text.contains("REMOVED"), "got: {text}");
        assert!(
            text.contains("re-run any soldr command"),
            "must tell the user the fix has taken effect, got: {text}"
        );
    });

    // The rule is "extensionless shadows .exe". Anything already carrying an
    // extension is not what a shell picks for bare `soldr`, so it can never
    // be the shadowing party.
    crate::timed_test!(only_an_extensionless_path_can_shadow, {
        assert_eq!(
            shadowed_sibling_path(Path::new("C:/tools/Scripts/soldr")),
            Some(PathBuf::from("C:/tools/Scripts/soldr.exe"))
        );
        assert_eq!(
            shadowed_sibling_path(Path::new("C:/tools/Scripts/soldr.exe")),
            None
        );
        assert_eq!(
            shadowed_sibling_path(Path::new("/usr/local/bin/soldr.sh")),
            None
        );
    });

    // Guards the name, not just the extension: the sibling must be the same
    // stem plus `.exe`, never a hardcoded "soldr.exe" -- the daemon alias
    // travels the same code path.
    crate::timed_test!(sibling_keeps_the_original_stem, {
        assert_eq!(
            shadowed_sibling_path(Path::new("/x/soldr-daemon")),
            Some(PathBuf::from("/x/soldr-daemon.exe"))
        );
    });

    crate::timed_test!(no_sibling_on_disk_is_not_shadowing, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let running = tmp.path().join("soldr");
        std::fs::write(&running, b"only-me").expect("write");
        assert_eq!(detect_shadowing_at(&running), None);
    });

    // The healthy steady state: soldr hardlinks its aliases, so the running
    // file and its .exe sibling are the same binary. Reporting that as a
    // problem would make the check pure noise on every correct install.
    crate::timed_test!(a_matching_sibling_is_the_healthy_case_not_a_warning, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let running = tmp.path().join("soldr");
        let installed = tmp.path().join("soldr.exe");
        std::fs::write(&running, b"same-bytes").expect("write");
        std::fs::write(&installed, b"same-bytes").expect("write");
        assert_eq!(detect_shadowing_at(&running), None);
    });

    crate::timed_test!(a_differing_sibling_is_reported_with_both_paths, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let running = tmp.path().join("soldr");
        let installed = tmp.path().join("soldr.exe");
        std::fs::write(&running, b"stale-old-build").expect("write");
        std::fs::write(&installed, b"fresh").expect("write");

        let found = detect_shadowing_at(&running).expect("shadowing must be detected");
        assert_eq!(found.running, running);
        assert_eq!(found.installed, installed);

        let report = shadowing_report(&found);
        assert!(report.contains("SHADOWED"), "{report}");
        // The remedy has to be in the message. This condition is invisible
        // otherwise -- that is the entire reason the check exists.
        assert!(report.contains("rm "), "{report}");
        assert!(report.contains(&running.display().to_string()), "{report}");
    });
}
