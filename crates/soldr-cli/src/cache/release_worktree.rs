//! `soldr cache release-worktree` / `soldr cache sweep-trash` — the
//! soldr-side fix for the Windows file-lock race documented in #710.
//!
//! On POSIX, deleting a directory with open file handles inside it
//! "just works" (`unlink(2)` survives via delete-on-close semantics).
//! On Windows, the OS refuses `unlink`/`rmdir` while any handle is
//! open, and long-lived caching daemons (zccache, rust-analyzer) hold
//! handles into the worktree's `target/` for seconds-to-minutes after
//! the build exits.
//!
//! This module implements **Tier 1 + Tier 2** of the three-tier design
//! in #710:
//!
//! 1. **Tier 1: inline recursive delete.** Try `remove_dir_all`. On
//!    POSIX this always succeeds, so the trash fallback is dead code
//!    there. On Windows it succeeds when no handles are open (the
//!    cold-teardown case).
//! 2. **Tier 2: rename-to-trash, same volume.** On Windows EACCES /
//!    EBUSY, `MoveFile` the worktree root to a per-volume trash dir
//!    (`~/.soldr/trash-<volume>/<timestamp>-<pid>/`). Per-volume is
//!    mandatory — cross-volume rename degrades to copy-delete and
//!    loses atomicity. Returns immediately; the user runs
//!    `soldr cache sweep-trash` (or future auto-GC hook) to reclaim
//!    the bytes once handles are released.
//! 3. **Tier 3: daemon-IPC release-handles** is deferred; it needs
//!    cooperation from zccache (out of repo). See #710 for the design.
//!
//! Per-volume trash is reusable from any caller that needs the same
//! escape hatch (the `clud-pr` skill being the first known user).

use crate::core::{SoldrError, SoldrPaths};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Outcome of one `release-worktree` invocation, both for direct human
/// output and the `--json` form consumed by the `clud-pr` skill.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReleaseOutcome {
    /// Tier 1 succeeded — the path no longer exists on disk.
    Removed { path: PathBuf },
    /// Tier 2 succeeded — the path was moved out of the way; the
    /// `trash_path` is where it lives now and is owned by the user's
    /// soldr trash directory.
    MovedToTrash {
        original: PathBuf,
        trash_path: PathBuf,
        tier1_error: String,
    },
    /// Both tiers failed. `tier1_error` and `tier2_error` are the OS
    /// error strings so the caller can decide whether to retry,
    /// surface a daemon-side fix prompt, or escalate.
    Failed {
        path: PathBuf,
        tier1_error: String,
        tier2_error: String,
    },
}

impl ReleaseOutcome {
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            ReleaseOutcome::Removed { .. } | ReleaseOutcome::MovedToTrash { .. }
        )
    }
}

/// Public entry point. Resolves the path, runs Tier 1, then Tier 2 if
/// Tier 1 failed. Trash dir layout is `~/.soldr/trash-<volume>/<id>/`
/// where `<volume>` is the drive letter on Windows (`C`, `D`) or the
/// device id stringified on Unix, and `<id>` is `<unix_nanos>-<pid>`
/// (sortable + unique without pulling in a uuid crate).
pub fn release_worktree(paths: &SoldrPaths, target: &Path) -> Result<ReleaseOutcome, SoldrError> {
    let canonical_or_self = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());

    // Tier 1.
    match std::fs::remove_dir_all(&canonical_or_self) {
        Ok(()) => Ok(ReleaseOutcome::Removed {
            path: canonical_or_self,
        }),
        Err(err) if is_busy_or_permission_error(&err) => {
            // Fall through to Tier 2.
            let tier1_error = err.to_string();
            tier2_rename_to_trash(paths, &canonical_or_self, tier1_error)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Already gone — treat as success, no work needed.
            Ok(ReleaseOutcome::Removed {
                path: canonical_or_self,
            })
        }
        Err(err) => {
            // Some other error (read-only fs, broken symlink, etc.) —
            // don't try Tier 2, it'd just fail too. Surface.
            Err(SoldrError::Other(format!(
                "release-worktree: tier-1 remove_dir_all({}) failed: {err}",
                canonical_or_self.display()
            )))
        }
    }
}

/// True if the error is one that Tier 2 (rename-to-trash) might
/// recover from — file-in-use, sharing violation, or "permission
/// denied" that's really a Windows handle-held condition.
fn is_busy_or_permission_error(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        err.kind(),
        ErrorKind::PermissionDenied | ErrorKind::Other | ErrorKind::ResourceBusy
    )
}

fn tier2_rename_to_trash(
    paths: &SoldrPaths,
    target: &Path,
    tier1_error: String,
) -> Result<ReleaseOutcome, SoldrError> {
    let volume = volume_tag_for_path(target);
    let trash_root = paths.root.join(format!("trash-{volume}"));
    std::fs::create_dir_all(&trash_root).map_err(|err| {
        SoldrError::Other(format!(
            "release-worktree: could not create trash root {}: {err}",
            trash_root.display()
        ))
    })?;
    let trash_target = trash_root.join(unique_trash_subdir_name());

    match std::fs::rename(target, &trash_target) {
        Ok(()) => Ok(ReleaseOutcome::MovedToTrash {
            original: target.to_path_buf(),
            trash_path: trash_target,
            tier1_error,
        }),
        Err(err) => Ok(ReleaseOutcome::Failed {
            path: target.to_path_buf(),
            tier1_error,
            tier2_error: err.to_string(),
        }),
    }
}

/// Per-volume tag for the trash directory name. On Windows: the
/// uppercase drive letter (`C`, `D`). On Unix: the device id from
/// `stat()`. Fall back to `default` when neither is available so we
/// don't accidentally cross-volume rename — the rename will fail and
/// we'll return `Failed`, which is the correct visible behavior.
#[cfg(windows)]
fn volume_tag_for_path(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = canonical.to_string_lossy().to_string();
    let trimmed = s.trim_start_matches(r"\\?\");
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return (bytes[0] as char).to_ascii_uppercase().to_string();
    }
    "default".to_string()
}

#[cfg(unix)]
fn volume_tag_for_path(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.dev().to_string())
        .unwrap_or_else(|| "default".to_string())
}

/// `<unix_nanos>-<pid>` — sortable, unique enough across processes,
/// no extra deps. Collisions within the same process within one
/// nanosecond are not a real concern for this use case.
fn unique_trash_subdir_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{nanos}-{pid}")
}

/// Tier 1 sweep of `~/.soldr/trash-*/` — recursive-delete every entry
/// that still exists. Tolerates per-entry failures (some entries may
/// still be daemon-held; we'll re-try next pass).
///
/// Returns `(removed, retained)` counts so the CLI can report progress.
pub fn sweep_trash(paths: &SoldrPaths) -> Result<SweepReport, SoldrError> {
    let root = &paths.root;
    let mut report = SweepReport::default();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(err) => return Err(SoldrError::Other(format!("sweep-trash: {err}"))),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("trash-") {
            continue;
        }
        let trash_root = entry.path();
        let bucket_entries = match std::fs::read_dir(&trash_root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for sub in bucket_entries.flatten() {
            let sub_path = sub.path();
            match std::fs::remove_dir_all(&sub_path) {
                Ok(()) => report.removed += 1,
                Err(_) => report.retained += 1,
            }
        }
    }
    Ok(report)
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct SweepReport {
    pub removed: u64,
    pub retained: u64,
}

pub fn run_cache_release_worktree_command(target: PathBuf, json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let outcome = release_worktree(&paths, &target)?;
    if json {
        super::print_json(&outcome)?;
    } else {
        match &outcome {
            ReleaseOutcome::Removed { path } => {
                println!("released (tier 1: removed) {}", path.display());
            }
            ReleaseOutcome::MovedToTrash {
                original,
                trash_path,
                tier1_error,
            } => {
                println!(
                    "released (tier 2: moved to trash) {} -> {}",
                    original.display(),
                    trash_path.display()
                );
                eprintln!("  tier-1 error was: {tier1_error}");
                eprintln!("  run `soldr cache sweep-trash` later to reclaim the bytes");
            }
            ReleaseOutcome::Failed {
                path,
                tier1_error,
                tier2_error,
            } => {
                eprintln!("failed to release {}", path.display());
                eprintln!("  tier-1 error: {tier1_error}");
                eprintln!("  tier-2 error: {tier2_error}");
                return Err(SoldrError::Other(format!(
                    "release-worktree: both tiers failed for {}",
                    path.display()
                )));
            }
        }
    }
    if !outcome.is_success() {
        return Err(SoldrError::Other(
            "release-worktree: did not succeed".to_string(),
        ));
    }
    Ok(())
}

pub fn run_cache_sweep_trash_command(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let report = sweep_trash(&paths)?;
    if json {
        super::print_json(&report)?;
    } else {
        println!(
            "sweep-trash: removed={} retained={}",
            report.removed, report.retained
        );
        if report.retained > 0 {
            eprintln!(
                "  ({} entries still daemon-held; re-run `sweep-trash` after the daemon idles)",
                report.retained
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn paths_for_root(root: &Path) -> SoldrPaths {
        SoldrPaths::with_root(root.to_path_buf())
    }

    crate::timed_test!(unique_trash_subdir_names_differ_across_consecutive_calls, {
        let a = unique_trash_subdir_name();
        // Sleep 1ns of busy work to defeat same-nanosecond collisions
        // on low-res clocks (Windows often resolves to 100ns).
        for _ in 0..1000 {
            std::hint::black_box(());
        }
        let b = unique_trash_subdir_name();
        assert_ne!(a, b, "expected unique names: a={a} b={b}");
    });

    crate::timed_test!(release_worktree_tier1_removes_empty_dir, {
        let scratch = tempdir().unwrap();
        let soldr_root = tempdir().unwrap();
        let target = scratch.path().join("doomed");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("a.txt"), b"contents").unwrap();
        let paths = paths_for_root(soldr_root.path());

        let outcome = release_worktree(&paths, &target).unwrap();
        assert!(matches!(outcome, ReleaseOutcome::Removed { .. }));
        assert!(!target.exists());
    });

    crate::timed_test!(release_worktree_tier1_treats_missing_path_as_success, {
        let scratch = tempdir().unwrap();
        let soldr_root = tempdir().unwrap();
        let target = scratch.path().join("never-existed");
        let paths = paths_for_root(soldr_root.path());

        let outcome = release_worktree(&paths, &target).unwrap();
        assert!(matches!(outcome, ReleaseOutcome::Removed { .. }));
    });

    crate::timed_test!(sweep_trash_returns_zero_on_empty_root, {
        let soldr_root = tempdir().unwrap();
        let paths = paths_for_root(soldr_root.path());
        let report = sweep_trash(&paths).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.retained, 0);
    });

    crate::timed_test!(sweep_trash_removes_per_volume_bucket_entries, {
        let soldr_root = tempdir().unwrap();
        let bucket = soldr_root.path().join("trash-X");
        let entry_a = bucket.join("100-1");
        let entry_b = bucket.join("200-2");
        std::fs::create_dir_all(&entry_a).unwrap();
        std::fs::create_dir_all(&entry_b).unwrap();
        std::fs::write(entry_a.join("file.txt"), b"x").unwrap();
        std::fs::write(entry_b.join("file.txt"), b"y").unwrap();
        // Decoy directory that doesn't match `trash-*` should be untouched.
        let decoy = soldr_root.path().join("cache");
        std::fs::create_dir(&decoy).unwrap();
        std::fs::write(decoy.join("keep.txt"), b"keep").unwrap();

        let paths = paths_for_root(soldr_root.path());
        let report = sweep_trash(&paths).unwrap();
        assert_eq!(report.removed, 2);
        assert_eq!(report.retained, 0);
        assert!(!entry_a.exists());
        assert!(!entry_b.exists());
        assert!(decoy.join("keep.txt").exists());
    });

    #[cfg(windows)]
    crate::timed_test!(volume_tag_for_windows_drive_letter_uppercases, {
        let p = Path::new(r"c:\Users\someone");
        let tag = volume_tag_for_path(p);
        // canonicalize will fail for a non-existent path; we fall back
        // to the input, so tag should still pick up "C".
        assert_eq!(tag, "C", "tag={tag}");
    });

    #[cfg(unix)]
    crate::timed_test!(volume_tag_for_unix_returns_device_id_string, {
        let scratch = tempdir().unwrap();
        let tag = volume_tag_for_path(scratch.path());
        assert!(
            tag.parse::<u64>().is_ok(),
            "expected numeric device id, got {tag}"
        );
    });

    crate::timed_test!(release_outcome_is_success_for_removed_and_moved, {
        assert!(ReleaseOutcome::Removed {
            path: PathBuf::from("/x"),
        }
        .is_success());
        assert!(ReleaseOutcome::MovedToTrash {
            original: PathBuf::from("/x"),
            trash_path: PathBuf::from("/y"),
            tier1_error: "busy".to_string(),
        }
        .is_success());
        assert!(!ReleaseOutcome::Failed {
            path: PathBuf::from("/x"),
            tier1_error: "busy".to_string(),
            tier2_error: "busy".to_string(),
        }
        .is_success());
    });
}
