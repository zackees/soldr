//! Displacement drain + ephemeral-image policy (soldr#2436 phase 3):
//! the D7 drain budget and the D8 disposable-build-dir guard. Split from
//! `mod.rs` for the #2493 1,000-line production ceiling.

use std::time::Duration;

/// D7 (soldr#2436): total wall-clock budget for an acknowledged
/// displacement drain before the kill fallback engages.
pub const DISPLACEMENT_DRAIN_TIMEOUT_ENV_VAR: &str = "SOLDR_DISPLACEMENT_DRAIN_TIMEOUT_SECS";
const DEFAULT_DISPLACEMENT_DRAIN_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) fn displacement_drain_timeout() -> Duration {
    std::env::var(DISPLACEMENT_DRAIN_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_DISPLACEMENT_DRAIN_TIMEOUT)
}

/// D8 (soldr#2436): a soldr binary running from a disposable build
/// directory must never initiate displacement of a live daemon — the
/// pip/uv build-env copies vanish minutes later, and a daemon they
/// displaced-and-replaced would hold the root from a deleted image
/// (the soldr#1987 orphan). Using an existing daemon, or spawning one
/// when none exists, stays allowed.
pub const ALLOW_EPHEMERAL_DISPLACE_ENV_VAR: &str = "SOLDR_ALLOW_EPHEMERAL_DISPLACE";

pub(crate) fn exe_path_is_ephemeral(path: &str) -> bool {
    ["pip-build-env-", "uv-build-env", ".tmp"]
        .iter()
        .any(|marker| path.contains(marker))
}

pub(crate) fn ephemeral_displacement_blocked() -> bool {
    if std::env::var(ALLOW_EPHEMERAL_DISPLACE_ENV_VAR)
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
    {
        return false;
    }
    std::env::current_exe()
        .ok()
        .map(|exe| exe_path_is_ephemeral(&exe.to_string_lossy()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_markers_match_disposable_build_dirs_only() {
        for path in [
            "/tmp/pip-build-env-a1b2/overlay/bin/soldr",
            r"C:\Users\u\AppData\Local\Temp\pip-build-env-xyz\Scripts\soldr.exe",
            "/home/u/.cache/uv/builds/uv-build-env-9f/bin/soldr",
            "/private/var/folders/x/T/build.tmp/soldr",
        ] {
            assert!(exe_path_is_ephemeral(path), "{path}");
        }
        for path in [
            "/usr/local/bin/soldr",
            r"C:\Users\u\.soldr\v0.9.1\shims\soldr.exe",
            "/home/u/dev/soldr/target/release/soldr",
        ] {
            assert!(!exe_path_is_ephemeral(path), "{path}");
        }
    }

    #[test]
    fn drain_budget_default_is_two_minutes() {
        assert_eq!(DEFAULT_DISPLACEMENT_DRAIN_TIMEOUT, Duration::from_secs(120));
    }
}
