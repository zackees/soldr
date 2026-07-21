//! Automatic pre- and post-compile `target/` prune hooks (issue #485 —
//! follow-up to #316). Wires the existing
//! [`super::prune_target::prune_target`] keep-latest strategy into the
//! `soldr cargo` front door so accumulated hash-suffixed orphans never
//! grow unbounded across rebuilds.
//!
//! The opt-out surface (flags + env var) lives in the front door; this
//! module only executes a single pass and returns a summary.

use super::prune_target::{prune_target, PruneTargetOptions};
use std::path::Path;

/// When the prune ran, relative to the cargo invocation. Used to label
/// the stderr summary so users can tell which pass freed which bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPrunePhase {
    Before,
    After,
}

impl AutoPrunePhase {
    pub fn label(self) -> &'static str {
        match self {
            AutoPrunePhase::Before => "before",
            AutoPrunePhase::After => "after",
        }
    }
}

/// Aggregated summary of one auto-prune pass. Empty when nothing was
/// deleted or when the pass was skipped — callers use [`Self::is_empty`]
/// to suppress the stderr line so silent operation stays silent.
#[derive(Debug, Default, Clone)]
pub struct AutoPruneOutcome {
    pub phase: Option<AutoPrunePhase>,
    pub deleted: usize,
    pub reclaimed_bytes: u64,
    /// True when the pass refused to run because an active `.cargo-lock`
    /// was present. Surfaced for tests; never printed by default since
    /// the issue spec asks for silence in that case.
    pub skipped_for_active_lock: bool,
}

impl AutoPruneOutcome {
    pub fn is_empty(&self) -> bool {
        self.deleted == 0 && self.reclaimed_bytes == 0 && !self.skipped_for_active_lock
    }
}

/// Best-effort prune of `target_dir` using the keep-latest strategy.
/// Silently skipped (returns an empty outcome) on:
///
/// * missing or non-directory `target_dir`,
/// * an active `.cargo-lock` (top-level or per-profile) — sets
///   `skipped_for_active_lock` so a parallel `cargo` invocation sharing
///   the same `target/` is never raced,
/// * any I/O failure during the walk.
///
/// The auto-hook MUST NEVER fail a build, so all error paths collapse
/// into "nothing happened this pass" rather than propagating to the
/// caller.
pub fn auto_prune_target(target_dir: &Path, phase: AutoPrunePhase) -> AutoPruneOutcome {
    let metadata = match std::fs::metadata(target_dir) {
        Ok(m) => m,
        Err(_) => return AutoPruneOutcome::default(),
    };
    if !metadata.is_dir() {
        return AutoPruneOutcome::default();
    }

    let opts = PruneTargetOptions {
        target_dir: target_dir.to_path_buf(),
        dry_run: false,
        keep_latest: true,
    };
    match prune_target(&opts) {
        Ok(report) => AutoPruneOutcome {
            phase: Some(phase),
            deleted: report.deleted,
            reclaimed_bytes: report.reclaimed_bytes,
            skipped_for_active_lock: false,
        },
        Err(err) => {
            // The prune_target error type stringifies the active-lock
            // refusal with a `.cargo-lock` substring; treat as a soft
            // skip rather than a build failure.
            let skipped_for_active_lock = err.to_string().contains(".cargo-lock");
            AutoPruneOutcome {
                phase: Some(phase),
                deleted: 0,
                reclaimed_bytes: 0,
                skipped_for_active_lock,
            }
        }
    }
}

/// Render a one-line stderr summary for a pass. Returns `None` when the
/// outcome is empty (no deletions, no skip) so callers can stay silent.
pub fn render_summary(outcome: &AutoPruneOutcome) -> Option<String> {
    if outcome.is_empty() {
        return None;
    }
    if outcome.skipped_for_active_lock {
        return None;
    }
    let phase = outcome.phase.map(|p| p.label()).unwrap_or("auto");
    Some(format!(
        "soldr: target-gc ({phase}): pruned {} stale hash {}, reclaimed {}",
        outcome.deleted,
        if outcome.deleted == 1 {
            "family"
        } else {
            "families"
        },
        super::target_registry::human_size(outcome.reclaimed_bytes),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;
    use std::fs::{self, File};
    use std::io::Write as _;
    use tempfile::tempdir;

    fn touch(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        File::create(path).unwrap();
    }

    fn hold_cargo_lock(path: &std::path::Path) -> File {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        file.try_lock_exclusive().unwrap();
        file
    }

    fn write_bytes(path: &std::path::Path, n: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = File::create(path).unwrap();
        let buf = vec![0u8; n];
        f.write_all(&buf).unwrap();
    }

    #[test]
    fn auto_prune_target_missing_dir_is_silent() {
        let temp = tempdir().unwrap();
        let absent = temp.path().join("does-not-exist");
        let outcome = auto_prune_target(&absent, AutoPrunePhase::Before);
        assert!(outcome.is_empty());
        assert!(render_summary(&outcome).is_none());
    }

    #[test]
    fn auto_prune_target_skips_for_active_lock() {
        let temp = tempdir().unwrap();
        let target = temp.path();
        touch(&target.join("debug/deps/libfoo-aaaaaaaaaaaaa.rlib"));
        touch(&target.join("debug/deps/libfoo-bbbbbbbbbbbbb.rlib"));
        let _cargo_lock = hold_cargo_lock(&target.join(".cargo-lock"));
        let outcome = auto_prune_target(target, AutoPrunePhase::Before);
        assert!(
            outcome.skipped_for_active_lock,
            "active lock must short-circuit"
        );
        // No stderr line on the active-lock skip per spec.
        assert!(render_summary(&outcome).is_none());
        // No files touched.
        assert!(target.join("debug/deps/libfoo-aaaaaaaaaaaaa.rlib").exists());
        assert!(target.join("debug/deps/libfoo-bbbbbbbbbbbbb.rlib").exists());
    }

    #[test]
    fn auto_prune_target_deletes_stale_hash_family() {
        let temp = tempdir().unwrap();
        let target = temp.path();
        let old = target.join("debug/deps/libfoo-aaaaaaaaaaaaa.rlib");
        let new = target.join("debug/deps/libfoo-bbbbbbbbbbbbb.rlib");
        write_bytes(&old, 4096);
        write_bytes(&new, 4096);
        // Pin entry mtimes (no fingerprint dir → falls back to entry mtime).
        let when_old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let when_new = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_500);
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(when_old)
            .unwrap();
        File::options()
            .write(true)
            .open(&new)
            .unwrap()
            .set_modified(when_new)
            .unwrap();

        let outcome = auto_prune_target(target, AutoPrunePhase::After);
        assert_eq!(outcome.deleted, 1);
        assert!(outcome.reclaimed_bytes > 0);
        assert_eq!(outcome.phase, Some(AutoPrunePhase::After));
        assert!(render_summary(&outcome).is_some());
        assert!(new.exists());
        assert!(!old.exists());
    }

    #[test]
    fn auto_prune_target_no_op_when_nothing_to_drop() {
        let temp = tempdir().unwrap();
        let target = temp.path();
        let single = target.join("debug/deps/libfoo-aaaaaaaaaaaaa.rlib");
        write_bytes(&single, 64);

        let outcome = auto_prune_target(target, AutoPrunePhase::Before);
        assert_eq!(outcome.deleted, 0);
        assert!(outcome.is_empty());
        assert!(render_summary(&outcome).is_none());
        assert!(single.exists());
    }

    #[test]
    fn summary_pluralizes_correctly() {
        let one = AutoPruneOutcome {
            phase: Some(AutoPrunePhase::Before),
            deleted: 1,
            reclaimed_bytes: 1024,
            skipped_for_active_lock: false,
        };
        assert!(render_summary(&one)
            .unwrap()
            .contains("1 stale hash family"));
        let many = AutoPruneOutcome {
            phase: Some(AutoPrunePhase::After),
            deleted: 3,
            reclaimed_bytes: 2048,
            skipped_for_active_lock: false,
        };
        assert!(render_summary(&many)
            .unwrap()
            .contains("3 stale hash families"));
    }
}
