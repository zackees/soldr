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
