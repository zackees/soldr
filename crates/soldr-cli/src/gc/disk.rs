//! Host-volume disk watchdog (issue #574).
//!
//! Probes free space on the volume that hosts the build's `target/` dir
//! (or CWD if `target/` doesn't exist yet) and either warns or blocks
//! the cargo invocation when free space drops below configurable
//! thresholds. Distinct from `cargo_front_door::disk` (which is the
//! older, narrower "low disk" advisory) — this is the GiB-scale
//! watchdog tied to cross-repo `target/` reclamation via
//! `soldr gc target`.

use std::io;
use std::path::{Path, PathBuf};

/// Env-var override for the warn threshold (in GiB).
pub(crate) const WARN_FREE_GB_ENV_VAR: &str = "SOLDR_TARGET_WARN_FREE_GB";
/// Env-var override for the block threshold (in GiB).
pub(crate) const BLOCK_FREE_GB_ENV_VAR: &str = "SOLDR_TARGET_BLOCK_FREE_GB";
/// Env-var that disables the watchdog entirely when set to a falsy
/// value (`0` / `false` / `no` / `off`, case-insensitive).
pub(crate) const AUTO_PRUNE_ENABLED_ENV_VAR: &str = "SOLDR_TARGET_AUTO_PRUNE_ENABLED";
/// Test seam — when set, overrides the real `fs2::available_space`
/// probe so unit tests can drive every threshold edge.
pub(crate) const TEST_DISK_FREE_BYTES_ENV_VAR: &str = "SOLDR_TEST_DISK_FREE_BYTES";

pub(crate) const DEFAULT_WARN_FREE_GB: u64 = 10;
pub(crate) const DEFAULT_BLOCK_FREE_GB: u64 = 5;

const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

/// Outcome of one watchdog check. Caller decides what to do — for
/// the cargo front door, `Warn` emits a one-line stderr message and
/// continues, `Block` aborts the invocation with a clear error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiskCheckOutcome {
    /// Watchdog disabled via `SOLDR_TARGET_AUTO_PRUNE_ENABLED=0`, or
    /// the disk probe failed. Either way the caller should proceed.
    Disabled,
    /// Free space is healthy.
    Ok { free_bytes: u64 },
    /// Free space dropped below the warn threshold but is still above
    /// the block threshold.
    Warn { free_bytes: u64, threshold_gib: u64 },
    /// Free space dropped below the block threshold. Caller MUST abort
    /// with the rendered suggestion line.
    Block { free_bytes: u64, threshold_gib: u64 },
}

/// Resolve free bytes for the disk that holds `path`. Honors
/// `SOLDR_TEST_DISK_FREE_BYTES` so unit tests can drive every code
/// path without touching the real filesystem.
pub(crate) fn free_bytes_for(path: &Path) -> io::Result<u64> {
    if let Some(raw) = std::env::var_os(TEST_DISK_FREE_BYTES_ENV_VAR) {
        let raw = raw.to_string_lossy();
        if raw.eq_ignore_ascii_case("error") {
            return Err(io::Error::other("test disk-space failure"));
        }
        return raw.parse::<u64>().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid {TEST_DISK_FREE_BYTES_ENV_VAR}: {e}"),
            )
        });
    }
    let probe = existing_probe_path(path);
    fs2::available_space(&probe)
}

/// Resolve the build volume for a cargo invocation. Prefers the
/// project's `target/` dir when it already exists (cargo will write
/// into it), falling back to the project's CWD when `target/` hasn't
/// been created yet. Callers pass `cwd` separately so tests can drive
/// both branches without changing the process CWD.
pub(crate) fn build_volume_path(cwd: &Path, target_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = target_dir {
        if dir.exists() {
            return dir.to_path_buf();
        }
    }
    cwd.to_path_buf()
}

/// Run the disk watchdog. Returns a [`DiskCheckOutcome`] describing
/// what (if anything) the caller should do.
pub(crate) fn check_disk_or_warn_or_block(workdir: &Path) -> DiskCheckOutcome {
    if !auto_prune_enabled() {
        return DiskCheckOutcome::Disabled;
    }
    let warn_gib = env_u64(WARN_FREE_GB_ENV_VAR).unwrap_or(DEFAULT_WARN_FREE_GB);
    let block_gib = env_u64(BLOCK_FREE_GB_ENV_VAR).unwrap_or(DEFAULT_BLOCK_FREE_GB);

    // Why: if the user inverts thresholds, prefer "block wins" so the
    // safety net is conservative rather than silently broken.
    let (warn_gib, block_gib) = if block_gib > warn_gib {
        (block_gib, block_gib)
    } else {
        (warn_gib, block_gib)
    };

    let free_bytes = match free_bytes_for(workdir) {
        Ok(b) => b,
        Err(_) => return DiskCheckOutcome::Disabled,
    };
    classify_outcome(free_bytes, warn_gib, block_gib)
}

fn classify_outcome(free_bytes: u64, warn_gib: u64, block_gib: u64) -> DiskCheckOutcome {
    let block_bytes = block_gib.saturating_mul(BYTES_PER_GIB);
    let warn_bytes = warn_gib.saturating_mul(BYTES_PER_GIB);
    if free_bytes < block_bytes {
        DiskCheckOutcome::Block {
            free_bytes,
            threshold_gib: block_gib,
        }
    } else if free_bytes < warn_bytes {
        DiskCheckOutcome::Warn {
            free_bytes,
            threshold_gib: warn_gib,
        }
    } else {
        DiskCheckOutcome::Ok { free_bytes }
    }
}

fn auto_prune_enabled() -> bool {
    match std::env::var_os(AUTO_PRUNE_ENABLED_ENV_VAR) {
        None => true,
        Some(value) => {
            let s = value.to_string_lossy();
            let t = s.trim();
            !(t.eq_ignore_ascii_case("0")
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("no")
                || t.eq_ignore_ascii_case("off")
                || t.is_empty())
        }
    }
}

fn env_u64(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    raw.trim().parse::<u64>().ok()
}

fn existing_probe_path(path: &Path) -> PathBuf {
    let mut cursor = if path.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        path.to_path_buf()
    };
    loop {
        if cursor.exists() {
            return cursor;
        }
        if !cursor.pop() {
            return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        }
    }
}

/// Render the one-line stderr warning emitted when the watchdog fires
/// at the `Warn` tier.
pub(crate) fn render_warn_line(free_bytes: u64, threshold_gib: u64) -> String {
    format!(
        "soldr: warning: build volume has {free} free (< {threshold_gib} GiB threshold). Run `soldr gc target` to find reclaimable `target/` directories under ~/dev.",
        free = format_bytes(free_bytes),
    )
}

/// Render the multi-line stderr error emitted when the watchdog fires
/// at the `Block` tier. Caller is expected to bail with a non-zero
/// exit code after printing. Returned without a leading `soldr: `
/// prefix — the top-level `report_and_exit` helper adds it.
pub(crate) fn render_block_message(free_bytes: u64, threshold_gib: u64) -> String {
    format!(
        "build volume has only {free} free, below the {threshold_gib} GiB block threshold. \
         Run `soldr gc target --purge` to reclaim space from cross-repo `target/` directories, or set \
         SOLDR_TARGET_BLOCK_FREE_GB=<lower> / SOLDR_TARGET_AUTO_PRUNE_ENABLED=0 to override.",
        free = format_bytes(free_bytes),
    )
}

fn format_bytes(bytes: u64) -> String {
    let gib = bytes as f64 / BYTES_PER_GIB as f64;
    if gib >= 1.0 {
        format!("{gib:.2} GiB")
    } else {
        let mib = bytes as f64 / (1024.0 * 1024.0);
        format!("{mib:.0} MiB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timed_test;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    timed_test!(threshold_classifier_returns_ok_above_warn, {
        let outcome = classify_outcome(
            20 * BYTES_PER_GIB,
            DEFAULT_WARN_FREE_GB,
            DEFAULT_BLOCK_FREE_GB,
        );
        assert!(matches!(outcome, DiskCheckOutcome::Ok { .. }));
    });

    timed_test!(threshold_classifier_returns_warn_between_thresholds, {
        let outcome = classify_outcome(
            7 * BYTES_PER_GIB,
            DEFAULT_WARN_FREE_GB,
            DEFAULT_BLOCK_FREE_GB,
        );
        match outcome {
            DiskCheckOutcome::Warn {
                threshold_gib,
                free_bytes,
            } => {
                assert_eq!(threshold_gib, DEFAULT_WARN_FREE_GB);
                assert_eq!(free_bytes, 7 * BYTES_PER_GIB);
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    });

    timed_test!(threshold_classifier_returns_block_below_block, {
        let outcome = classify_outcome(
            2 * BYTES_PER_GIB,
            DEFAULT_WARN_FREE_GB,
            DEFAULT_BLOCK_FREE_GB,
        );
        match outcome {
            DiskCheckOutcome::Block {
                threshold_gib,
                free_bytes,
            } => {
                assert_eq!(threshold_gib, DEFAULT_BLOCK_FREE_GB);
                assert_eq!(free_bytes, 2 * BYTES_PER_GIB);
            }
            other => panic!("expected Block, got {other:?}"),
        }
    });

    timed_test!(threshold_classifier_block_wins_at_block_boundary, {
        let outcome = classify_outcome(DEFAULT_BLOCK_FREE_GB * BYTES_PER_GIB - 1, 10, 5);
        assert!(matches!(outcome, DiskCheckOutcome::Block { .. }));
    });

    timed_test!(check_disk_disabled_via_env_var, {
        let _lock = ENV_LOCK.lock().unwrap();
        let _disabled = EnvVarGuard::set(AUTO_PRUNE_ENABLED_ENV_VAR, "0");
        let _free = EnvVarGuard::set(TEST_DISK_FREE_BYTES_ENV_VAR, "0");
        let outcome = check_disk_or_warn_or_block(std::path::Path::new("."));
        assert!(matches!(outcome, DiskCheckOutcome::Disabled));
    });

    timed_test!(check_disk_warns_with_custom_threshold, {
        let _lock = ENV_LOCK.lock().unwrap();
        let _enabled = EnvVarGuard::remove(AUTO_PRUNE_ENABLED_ENV_VAR);
        let _warn = EnvVarGuard::set(WARN_FREE_GB_ENV_VAR, "20");
        let _block = EnvVarGuard::set(BLOCK_FREE_GB_ENV_VAR, "5");
        let _free = EnvVarGuard::set(
            TEST_DISK_FREE_BYTES_ENV_VAR,
            &(15u64 * BYTES_PER_GIB).to_string(),
        );
        let outcome = check_disk_or_warn_or_block(std::path::Path::new("."));
        match outcome {
            DiskCheckOutcome::Warn { threshold_gib, .. } => assert_eq!(threshold_gib, 20),
            other => panic!("expected Warn, got {other:?}"),
        }
    });

    timed_test!(check_disk_blocks_with_custom_threshold, {
        let _lock = ENV_LOCK.lock().unwrap();
        let _enabled = EnvVarGuard::remove(AUTO_PRUNE_ENABLED_ENV_VAR);
        let _warn = EnvVarGuard::set(WARN_FREE_GB_ENV_VAR, "20");
        let _block = EnvVarGuard::set(BLOCK_FREE_GB_ENV_VAR, "10");
        let _free = EnvVarGuard::set(
            TEST_DISK_FREE_BYTES_ENV_VAR,
            &(3u64 * BYTES_PER_GIB).to_string(),
        );
        let outcome = check_disk_or_warn_or_block(std::path::Path::new("."));
        match outcome {
            DiskCheckOutcome::Block { threshold_gib, .. } => assert_eq!(threshold_gib, 10),
            other => panic!("expected Block, got {other:?}"),
        }
    });

    timed_test!(check_disk_disabled_when_probe_errors, {
        let _lock = ENV_LOCK.lock().unwrap();
        let _enabled = EnvVarGuard::remove(AUTO_PRUNE_ENABLED_ENV_VAR);
        let _free = EnvVarGuard::set(TEST_DISK_FREE_BYTES_ENV_VAR, "error");
        let outcome = check_disk_or_warn_or_block(std::path::Path::new("."));
        assert!(matches!(outcome, DiskCheckOutcome::Disabled));
    });

    timed_test!(inverted_thresholds_collapse_to_block_value, {
        let _lock = ENV_LOCK.lock().unwrap();
        let _enabled = EnvVarGuard::remove(AUTO_PRUNE_ENABLED_ENV_VAR);
        // warn=5, block=10 (inverted) — should collapse to a single
        // block-only check at 10 GiB.
        let _warn = EnvVarGuard::set(WARN_FREE_GB_ENV_VAR, "5");
        let _block = EnvVarGuard::set(BLOCK_FREE_GB_ENV_VAR, "10");
        let _free = EnvVarGuard::set(
            TEST_DISK_FREE_BYTES_ENV_VAR,
            &(7u64 * BYTES_PER_GIB).to_string(),
        );
        let outcome = check_disk_or_warn_or_block(std::path::Path::new("."));
        assert!(matches!(outcome, DiskCheckOutcome::Block { .. }));
    });

    timed_test!(build_volume_path_prefers_target_when_present, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path();
        let target = cwd.join("target");
        std::fs::create_dir_all(&target).expect("create target");
        let resolved = build_volume_path(cwd, Some(&target));
        assert_eq!(resolved, target);
    });

    timed_test!(build_volume_path_falls_back_to_cwd_when_target_missing, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path();
        let target = cwd.join("target");
        let resolved = build_volume_path(cwd, Some(&target));
        assert_eq!(resolved, cwd);
    });

    timed_test!(warn_line_includes_threshold_and_command, {
        let line = render_warn_line(7 * BYTES_PER_GIB, 10);
        assert!(line.contains("7.00 GiB"));
        assert!(line.contains("10 GiB"));
        assert!(line.contains("soldr gc target"));
    });

    timed_test!(block_line_directs_user_at_purge, {
        let msg = render_block_message(2 * BYTES_PER_GIB, 5);
        assert!(msg.contains("2.00 GiB"));
        assert!(msg.contains("5 GiB"));
        assert!(msg.contains("soldr gc target --purge"));
        assert!(msg.contains("SOLDR_TARGET_BLOCK_FREE_GB"));
        // Why: `report_and_exit` already prepends "soldr: " — the
        // rendered message must not double it.
        assert!(!msg.starts_with("soldr:"));
    });
}
