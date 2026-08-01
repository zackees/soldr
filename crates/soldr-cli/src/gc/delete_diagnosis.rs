//! What survived a failed recursive delete, and what is unusual about it.
//!
//! soldr#2199: `gc target --purge` failed on two trees with
//! `The directory is not empty. (os error 145)`. That error is reported by the
//! *parent* and names nothing; the real refusal happened on some leaf inside
//! it and is not surfaced anywhere. Diagnosing it took reproducing the delete
//! by hand, on a machine that had already dropped to 10.5 GB free.
//!
//! The root cause is still unknown. The read-only theory in the original
//! report was disproven — Rust's `remove_dir_all` passes
//! `FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE`, so the attribute cannot be
//! what blocked it — which leaves an open handle, a reparse point, or a path
//! the call could not address. This module does not guess between them: it
//! counts the evidence that tells them apart, so the *next* occurrence is
//! diagnosable from the log instead of from a live debugging session.
//!
//! Deliberately read-only. It runs on a path that has already failed, so it
//! must not be able to make things worse.

use std::path::{Path, PathBuf};

/// A census of the entries still present under a directory.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Survivors {
    /// Files, directories and links still on disk.
    pub(super) entries: usize,
    /// Entries carrying the read-only attribute. Reported to *settle* the
    /// question rather than to act on it: if a failing tree shows zero, the
    /// disproof holds on the reporting machine too, and nobody has to
    /// re-litigate it from first principles again.
    pub(super) read_only: usize,
    /// Symlinks and Windows reparse points (junctions). Never descended into
    /// — a junction under `target/` is one of the open candidate causes, and
    /// following it could walk clean out of the tree.
    pub(super) links: usize,
    /// The longest full path seen, and its length. `LongPathsEnabled` was set
    /// on the reporting box and the deepest path was 189 characters, but that
    /// was measured before the failure rather than on the surviving set.
    pub(super) longest: Option<(PathBuf, usize)>,
}

impl Survivors {
    /// One line for the failure log, or `None` when nothing survived.
    ///
    /// Nothing surviving is itself worth knowing: it means the delete
    /// completed after the error was returned, which points at a transient
    /// hold rather than anything structural.
    pub(super) fn summarize(&self) -> Option<String> {
        if self.entries == 0 {
            return None;
        }
        let mut line = if self.entries == 1 {
            "1 entry remains".to_string()
        } else {
            format!("{} entries remain", self.entries)
        };
        if self.read_only > 0 {
            line.push_str(&format!(", {} read-only", self.read_only));
        }
        if self.links > 0 {
            line.push_str(&format!(", {} link/reparse point", self.links));
        }
        if let Some((path, len)) = &self.longest {
            line.push_str(&format!(", longest path {len} chars: {}", path.display()));
        }
        Some(line)
    }
}

/// Walk `dir` without following links, counting what is still there.
///
/// Unreadable subdirectories are counted and skipped rather than aborting the
/// walk: a permission error partway through is a finding, not a reason to
/// return nothing.
pub(super) fn survey(dir: &Path) -> Survivors {
    let mut survivors = Survivors::default();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            survivors.entries += 1;

            let path_len = path.as_os_str().len();
            if survivors
                .longest
                .as_ref()
                .is_none_or(|(_, len)| path_len > *len)
            {
                survivors.longest = Some((path.clone(), path_len));
            }

            // `symlink_metadata` so a link is described as itself rather than
            // as whatever it points at.
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.permissions().readonly() {
                survivors.read_only += 1;
            }
            if metadata.file_type().is_symlink() {
                survivors.links += 1;
                continue;
            }
            if metadata.is_dir() {
                stack.push(path);
            }
        }
    }
    survivors
}

/// [`survey`] rendered for a log line, skipping the walk when `dir` is gone.
///
/// soldr#2199: the census says what survived; a running process mapped from
/// the tree says *why*, and is the only measured cause of the error the
/// original report saw. Name it when there is one -- "pid 4321 (held.exe) is
/// running from this tree" is something a user can act on, where "3 entries
/// remain" is not.
pub(super) fn describe(dir: &Path) -> Option<String> {
    if !dir.exists() {
        return None;
    }
    let census = survey(dir).summarize();
    let holders = super::holding_process::summarize(&super::holding_process::holders_under(dir));
    match (census, holders) {
        (Some(census), Some(holders)) => Some(format!("{census}; {holders}")),
        (Some(census), None) => Some(census),
        // Worth reporting even with an empty census: a holder explains a
        // failure whose tree has since drained.
        (None, Some(holders)) => Some(holders),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    crate::timed_test!(a_missing_directory_produces_no_diagnosis, {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("never-existed");
        assert!(describe(&gone).is_none());
    });

    crate::timed_test!(an_empty_directory_produces_no_diagnosis, {
        // The delete finished after reporting an error. Saying "0 entries
        // remain" would read as a finding; saying nothing is the finding.
        let dir = tempfile::tempdir().unwrap();
        assert!(describe(dir.path()).is_none());
    });

    crate::timed_test!(survivors_are_counted_through_subdirectories, {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.rlib"), "x");
        write(&dir.path().join("deps/b.rlib"), "y");
        write(&dir.path().join("deps/nested/c.rmeta"), "z");

        let survivors = survey(dir.path());
        // 3 files + 2 directories.
        assert_eq!(survivors.entries, 5, "{survivors:?}");
        assert_eq!(survivors.links, 0);
    });

    crate::timed_test!(the_read_only_count_is_reported, {
        // The number that settles the disproven theory on a real failing
        // tree, rather than requiring it be argued again.
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("libsoldr_cache.rlib");
        write(&locked, "x");
        let mut perms = std::fs::metadata(&locked).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&locked, perms).unwrap();

        let survivors = survey(dir.path());
        assert_eq!(survivors.read_only, 1, "{survivors:?}");
        let summary = survivors.summarize().unwrap();
        assert!(summary.contains("1 read-only"), "{summary}");

        // No cleanup: `TempDir`'s drop uses `remove_dir_all`, which deletes a
        // read-only file on Windows and on Unix alike. If that ever stops
        // being true this test leaks a directory -- which is the loudest
        // available signal that the premise behind #2200 came back.
    });

    crate::timed_test!(the_longest_path_is_named, {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("short"), "x");
        let deep = dir.path().join("a/bb/ccc/a-considerably-longer-name.rlib");
        write(&deep, "y");

        let survivors = survey(dir.path());
        let (path, len) = survivors.longest.clone().unwrap();
        assert_eq!(path, deep);
        assert_eq!(len, deep.as_os_str().len());
        let summary = survivors.summarize().unwrap();
        assert!(summary.contains("longest path"), "{summary}");
    });

    crate::timed_test!(the_count_leads_and_agrees_with_itself, {
        // The line goes in front of a user at the moment a purge failed, so
        // it should not also be ungrammatical.
        let one = Survivors {
            entries: 1,
            ..Default::default()
        };
        assert_eq!(one.summarize().unwrap(), "1 entry remains");

        let many = Survivors {
            entries: 12_483,
            read_only: 6,
            ..Default::default()
        };
        assert_eq!(
            many.summarize().unwrap(),
            "12483 entries remain, 6 read-only"
        );
    });
}
