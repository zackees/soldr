//! Empirical Windows Defender real-time scan probe (issue #357).
//!
//! Defender exclusion lists are admin-only (`Get-MpPreference
//! -ExclusionPath` returns `N/A: Must be an administrator to view
//! exclusions` for non-admin processes). To tell whether the soldr
//! cache directory is being scanned from a non-admin shell we use an
//! empirical workaround: write a small file with a Defender-watched
//! extension into the target directory, time the syscall, then delete
//! it. Excluded paths complete in single-digit ms; scanned paths take
//! tens to hundreds of ms because Defender synchronously inspects the
//! write.
//!
//! The probe is throttled — running it on every `soldr cargo build`
//! would add 50-500ms to the hot path. State lives in
//! `~/.soldr/defender-probe.json` and is refreshed on:
//!
//! 1. State file missing.
//! 2. `probed_at_unix` older than 7 days.
//! 3. `probed_path` doesn't match the current `SOLDR_CACHE_DIR`.
//! 4. `soldr_version` doesn't match (probe logic may have changed).
//! 5. Explicit `soldr doctor --refresh-defender-probe` flag.
//!
//! Surfaces via `soldr doctor`. On non-Windows the entire module is a
//! pure no-op stub: probe verdict reads `not_applicable` and the
//! probe function never touches disk.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::{SoldrError, SoldrPaths};

/// Probe is throttled so we don't pay the 50-500 ms write cost on every
/// soldr invocation. Re-runs only after this interval (or on path /
/// version change / explicit refresh).
pub const PROBE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// File-size of the synthetic write used by the probe. 1 MiB is large
/// enough that Defender's synchronous scan is measurable above noise,
/// and small enough that the write completes in under a second even on
/// a busy hosted runner.
pub const PROBE_FILE_SIZE_BYTES: usize = 1024 * 1024;

/// Number of times the probe writes the synthetic file. We take the
/// median to suppress single-syscall outliers.
pub const PROBE_REPEATS: usize = 3;

/// Median write time (ms) at or above which we classify the path as
/// `scanned`. Tuned empirically — single-digit ms is consistent with
/// excluded paths; ~80 ms is the floor for real-time scanning.
pub const SCANNED_THRESHOLD_MS: u64 = 80;

/// File extension used for the probe write. `.dll` is high-attention
/// for Defender; `.exe` / `.ps1` would also work. The synthetic file
/// is deleted immediately so the extension only matters for how the
/// scanner classifies the write event.
pub const PROBE_FILE_EXTENSION: &str = "dll";

/// Schema version baked into the cached probe state — if we change
/// the probe shape we bump this and the cache reader forces a refresh.
pub const PROBE_SCHEMA_VERSION: u32 = 1;

/// Classification of a single probe run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefenderVerdict {
    /// Median write time is at or above the scanned threshold.
    Scanned,
    /// Median write time is below the scanned threshold; the path
    /// appears to be excluded from real-time scanning, on a trusted
    /// Dev Drive, or otherwise not subject to per-write inspection.
    Excluded,
    /// Probe was not run because the platform does not support
    /// Windows Defender (macOS, Linux).
    NotApplicable,
}

impl DefenderVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            DefenderVerdict::Scanned => "scanned",
            DefenderVerdict::Excluded => "excluded",
            DefenderVerdict::NotApplicable => "not_applicable",
        }
    }
}

/// Persistent state file written to `~/.soldr/defender-probe.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefenderProbeState {
    pub schema_version: u32,
    pub probed_at_unix: u64,
    pub probed_path: PathBuf,
    pub median_write_ms: u64,
    pub verdict: DefenderVerdict,
    pub platform: String,
    pub soldr_version: String,
}

/// Reasons the probe state needs refreshing. Returned by
/// [`reprobe_reason`] so callers (and tests) can branch on the cause
/// without re-implementing the precedence rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReprobeReason {
    /// No cached state on disk.
    StateMissing,
    /// State is older than [`PROBE_TTL`].
    StaleByAge,
    /// Cached probe targeted a different cache directory.
    PathChanged,
    /// Soldr version changed since the cached probe — re-run in case
    /// the probe logic itself changed.
    VersionChanged,
    /// Schema version baked into the state file is older than
    /// [`PROBE_SCHEMA_VERSION`].
    SchemaChanged,
    /// Caller forced a refresh (`--refresh-defender-probe`).
    Forced,
}

/// Decide whether the cached state needs to be refreshed.
///
/// Returns `None` when the cached state is still valid for the
/// requested path + version. Returns `Some(reason)` describing why
/// the probe should be re-run. The function is pure so callers can
/// unit-test the precedence rules without touching the filesystem.
pub fn reprobe_reason(
    state: Option<&DefenderProbeState>,
    current_path: &Path,
    current_version: &str,
    now_unix: u64,
    forced: bool,
) -> Option<ReprobeReason> {
    if forced {
        return Some(ReprobeReason::Forced);
    }
    let Some(state) = state else {
        return Some(ReprobeReason::StateMissing);
    };
    if state.schema_version < PROBE_SCHEMA_VERSION {
        return Some(ReprobeReason::SchemaChanged);
    }
    if state.probed_path != current_path {
        return Some(ReprobeReason::PathChanged);
    }
    if state.soldr_version != current_version {
        return Some(ReprobeReason::VersionChanged);
    }
    if now_unix.saturating_sub(state.probed_at_unix) >= PROBE_TTL.as_secs() {
        return Some(ReprobeReason::StaleByAge);
    }
    None
}

/// Pure classifier: a median write time at or above
/// [`SCANNED_THRESHOLD_MS`] is scanned, below is excluded.
pub fn classify_median_ms(median_ms: u64) -> DefenderVerdict {
    if median_ms >= SCANNED_THRESHOLD_MS {
        DefenderVerdict::Scanned
    } else {
        DefenderVerdict::Excluded
    }
}

/// Path to the cached probe state file.
pub fn probe_state_path(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("defender-probe.json")
}

/// Load the cached probe state. Returns `None` when the file is
/// missing, unreadable, or fails to parse — callers treat any of
/// those as "needs reprobe".
pub fn read_probe_state(paths: &SoldrPaths) -> Option<DefenderProbeState> {
    let path = probe_state_path(paths);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist the probe state file. Best-effort — a write failure is
/// non-fatal and surfaced to the caller as an `Err` so it can log
/// without aborting the doctor command.
pub fn write_probe_state(paths: &SoldrPaths, state: &DefenderProbeState) -> Result<(), SoldrError> {
    let path = probe_state_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(state)
        .map_err(|e| SoldrError::Other(format!("failed to serialize defender-probe state: {e}")))?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Current platform tag baked into the state file.
pub fn platform_tag() -> &'static str {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        "win32"
    } else if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::MacOs {
        "darwin"
    } else if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux {
        "linux"
    } else {
        "unknown"
    }
}

/// Run the probe against `target_dir` and return a fresh state record.
///
/// On non-Windows this is a pure no-op: returns `NotApplicable` with
/// `median_write_ms = 0`. On Windows: writes a 1 MiB `.dll` file
/// [`PROBE_REPEATS`] times, takes the median wall-clock time, and
/// classifies the median against [`SCANNED_THRESHOLD_MS`].
pub fn run_probe(target_dir: &Path, soldr_version: &str) -> Result<DefenderProbeState, SoldrError> {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        return Ok(DefenderProbeState {
            schema_version: PROBE_SCHEMA_VERSION,
            probed_at_unix: now_unix,
            probed_path: target_dir.to_path_buf(),
            median_write_ms: 0,
            verdict: DefenderVerdict::NotApplicable,
            platform: platform_tag().to_string(),
            soldr_version: soldr_version.to_string(),
        });
    }

    let median_ms = measure_median_write_ms(target_dir)?;
    Ok(DefenderProbeState {
        schema_version: PROBE_SCHEMA_VERSION,
        probed_at_unix: now_unix,
        probed_path: target_dir.to_path_buf(),
        median_write_ms: median_ms,
        verdict: classify_median_ms(median_ms),
        platform: platform_tag().to_string(),
        soldr_version: soldr_version.to_string(),
    })
}

/// Write 1 MiB of pseudo-random bytes into a unique `.dll` file inside
/// `target_dir`, time the full `open + write + flush + close` cycle,
/// then delete the file. Repeat [`PROBE_REPEATS`] times; return the
/// median wall-clock duration in milliseconds.
///
/// Errors only if `target_dir` cannot be created. Individual write
/// failures during the loop are converted to a large sample so a
/// permission glitch never masks a "scanned" verdict.
fn measure_median_write_ms(target_dir: &Path) -> Result<u64, SoldrError> {
    std::fs::create_dir_all(target_dir)?;
    let mut samples: Vec<u64> = Vec::with_capacity(PROBE_REPEATS);

    // Cheap, deterministic-ish pseudo-random bytes. We don't need
    // crypto — Defender's scanner looks at file content patterns
    // and a fixed buffer would let the scanner short-circuit. Mix in
    // wall-clock nanos so each invocation has a different fingerprint.
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0xC0FFEE);
    let mut buffer = vec![0u8; PROBE_FILE_SIZE_BYTES];
    fill_pseudo_random(&mut buffer, seed);

    for idx in 0..PROBE_REPEATS {
        let name = format!(
            "soldr-defender-probe-{}-{}.{}",
            std::process::id(),
            idx,
            PROBE_FILE_EXTENSION
        );
        let path = target_dir.join(&name);
        let elapsed = match timed_write(&path, &buffer) {
            Ok(ms) => ms,
            Err(_) => {
                // Permission glitch or transient failure — record an
                // upper-bound sample so we err toward "scanned".
                u64::from(u16::MAX)
            }
        };
        samples.push(elapsed);
        let _ = std::fs::remove_file(&path);
    }

    samples.sort_unstable();
    Ok(samples[samples.len() / 2])
}

fn timed_write(path: &Path, buffer: &[u8]) -> Result<u64, std::io::Error> {
    use std::io::Write;
    let started = std::time::Instant::now();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(buffer)?;
    file.flush()?;
    // Drop the handle explicitly so the close happens before we stop
    // the timer — Defender's synchronous scan can land on close.
    drop(file);
    Ok(started.elapsed().as_millis() as u64)
}

/// Splittable random-ish fill: lightweight xorshift seeded by the
/// wall clock. We don't need statistical-grade randomness — just
/// content that the scanner can't trivially fingerprint as "soldr
/// already touched this last invocation."
fn fill_pseudo_random(buffer: &mut [u8], mut seed: u64) {
    if seed == 0 {
        seed = 0x9E3779B97F4A7C15;
    }
    for chunk in buffer.chunks_mut(8) {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let bytes = seed.to_le_bytes();
        for (slot, byte) in chunk.iter_mut().zip(bytes.iter()) {
            *slot = *byte;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh_state(probed_path: PathBuf, soldr_version: &str, age_secs: u64) -> DefenderProbeState {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        DefenderProbeState {
            schema_version: PROBE_SCHEMA_VERSION,
            probed_at_unix: now.saturating_sub(age_secs),
            probed_path,
            median_write_ms: 12,
            verdict: DefenderVerdict::Excluded,
            platform: platform_tag().to_string(),
            soldr_version: soldr_version.to_string(),
        }
    }

    #[test]
    fn classify_median_threshold_boundary() {
        // The threshold is inclusive on the "scanned" side: meeting
        // exactly SCANNED_THRESHOLD_MS already means Defender's
        // synchronous scan is touching the path.
        assert_eq!(
            classify_median_ms(SCANNED_THRESHOLD_MS),
            DefenderVerdict::Scanned
        );
        assert_eq!(
            classify_median_ms(SCANNED_THRESHOLD_MS - 1),
            DefenderVerdict::Excluded
        );
        assert_eq!(classify_median_ms(0), DefenderVerdict::Excluded);
        assert_eq!(classify_median_ms(500), DefenderVerdict::Scanned);
    }

    #[test]
    fn reprobe_when_state_missing() {
        let reason = reprobe_reason(None, Path::new("/x"), "0.0.0", 0, false);
        assert_eq!(reason, Some(ReprobeReason::StateMissing));
    }

    #[test]
    fn no_reprobe_when_state_is_fresh_and_matches() {
        let state = fresh_state(PathBuf::from("/x"), "0.7.31", 60);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reason = reprobe_reason(Some(&state), Path::new("/x"), "0.7.31", now, false);
        assert_eq!(reason, None);
    }

    #[test]
    fn reprobe_when_age_exceeds_ttl() {
        // Pretend cached state is 8 days old.
        let state = fresh_state(PathBuf::from("/x"), "0.7.31", 8 * 24 * 60 * 60);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reason = reprobe_reason(Some(&state), Path::new("/x"), "0.7.31", now, false);
        assert_eq!(reason, Some(ReprobeReason::StaleByAge));
    }

    #[test]
    fn reprobe_when_path_changed() {
        let state = fresh_state(PathBuf::from("/old"), "0.7.31", 60);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reason = reprobe_reason(Some(&state), Path::new("/new"), "0.7.31", now, false);
        assert_eq!(reason, Some(ReprobeReason::PathChanged));
    }

    #[test]
    fn reprobe_when_version_changed() {
        let state = fresh_state(PathBuf::from("/x"), "0.7.30", 60);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reason = reprobe_reason(Some(&state), Path::new("/x"), "0.7.31", now, false);
        assert_eq!(reason, Some(ReprobeReason::VersionChanged));
    }

    #[test]
    fn reprobe_when_schema_is_older() {
        let mut state = fresh_state(PathBuf::from("/x"), "0.7.31", 60);
        state.schema_version = PROBE_SCHEMA_VERSION.saturating_sub(1);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reason = reprobe_reason(Some(&state), Path::new("/x"), "0.7.31", now, false);
        assert_eq!(reason, Some(ReprobeReason::SchemaChanged));
    }

    #[test]
    fn reprobe_forced_overrides_every_other_check() {
        // Forced refresh: even with a fresh-and-matching cached state
        // we still re-run. The reason returned is `Forced` so callers
        // can render it distinctly from "stale by age".
        let state = fresh_state(PathBuf::from("/x"), "0.7.31", 60);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reason = reprobe_reason(Some(&state), Path::new("/x"), "0.7.31", now, true);
        assert_eq!(reason, Some(ReprobeReason::Forced));
    }

    #[test]
    fn run_probe_on_non_windows_is_a_noop_returning_not_applicable() {
        // The probe is OS-agnostic compile-time; on non-Windows we
        // short-circuit before touching disk. This test runs on every
        // platform — on Windows it actually runs the probe and we just
        // assert the state has the right shape.
        let tmp = tempdir().unwrap();
        let state = run_probe(tmp.path(), "0.7.31").unwrap();
        assert_eq!(state.schema_version, PROBE_SCHEMA_VERSION);
        assert_eq!(state.probed_path, tmp.path());
        assert_eq!(state.soldr_version, "0.7.31");
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            // We don't assert the verdict because the test runner's
            // exclusion state is environmental, but the median must
            // be a real number.
            assert!(matches!(
                state.verdict,
                DefenderVerdict::Scanned | DefenderVerdict::Excluded
            ));
        } else {
            assert_eq!(state.verdict, DefenderVerdict::NotApplicable);
            assert_eq!(state.median_write_ms, 0);
        }
    }

    #[test]
    fn state_round_trips_through_disk() {
        let tmp = tempdir().unwrap();
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let state = DefenderProbeState {
            schema_version: PROBE_SCHEMA_VERSION,
            probed_at_unix: 1_700_000_000,
            probed_path: PathBuf::from("/some/cache/dir"),
            median_write_ms: 412,
            verdict: DefenderVerdict::Scanned,
            platform: "win32".to_string(),
            soldr_version: "0.7.31".to_string(),
        };
        write_probe_state(&paths, &state).unwrap();
        let round = read_probe_state(&paths).unwrap();
        assert_eq!(round.probed_path, state.probed_path);
        assert_eq!(round.median_write_ms, state.median_write_ms);
        assert_eq!(round.verdict, state.verdict);
        assert_eq!(round.soldr_version, state.soldr_version);
    }

    #[test]
    fn read_probe_state_returns_none_when_missing() {
        let tmp = tempdir().unwrap();
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        assert!(read_probe_state(&paths).is_none());
    }

    #[test]
    fn read_probe_state_returns_none_when_malformed() {
        let tmp = tempdir().unwrap();
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        std::fs::write(probe_state_path(&paths), b"not json").unwrap();
        assert!(read_probe_state(&paths).is_none());
    }

    #[test]
    fn pseudo_random_fill_is_deterministic_for_same_seed() {
        let mut a = vec![0u8; 64];
        let mut b = vec![0u8; 64];
        fill_pseudo_random(&mut a, 12345);
        fill_pseudo_random(&mut b, 12345);
        assert_eq!(a, b);
        // ...and different seeds give different output.
        let mut c = vec![0u8; 64];
        fill_pseudo_random(&mut c, 54321);
        assert_ne!(a, c);
    }
}
