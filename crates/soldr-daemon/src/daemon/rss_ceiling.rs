//! Optional daemon RSS ceiling (soldr#3038).
//!
//! A canonical `soldr-daemon` reached 11.7 GiB of private anonymous memory
//! on production hardware with no instrumentation to say whether that was
//! live data or allocator retention. This module is the test-facing half of
//! the fix: an opt-in watchdog that samples the daemon's own resident set on
//! a short cadence and records a legible, file-based verdict — never a
//! process exit, which would fail whatever build is in flight with a
//! confusing "daemon unavailable" error instead of naming the real cause.
//!
//! ## Design decisions
//!
//! - **Env var, not a config field.** `config.toml` is meant for durable,
//!   human-authored policy; this is a diagnostic knob a CI job or a
//!   developer flips for one run. Every other single-run diagnostic knob in
//!   this codebase (`SOLDR_TEST_DISK_FREE_BYTES`,
//!   `SOLDR_COMPILE_REPLY_TIMEOUT_SECS`, ...) is an env var — see
//!   `docs/API.md`'s Environment Variables table.
//! - **Unset means genuinely no behaviour change.** [`ceiling_bytes_from_env`]
//!   is the only thing that runs unconditionally, and it is one env lookup.
//!   The sampling task itself is spawned only when a positive ceiling is
//!   configured (see `server_runtime.rs`), so an ordinary build pays no
//!   extra timer, no extra file write, no extra `mimalloc_pprof::prof::stats()`
//!   call.
//! - **A dedicated, fast cadence — not the existing 5-minute maintenance
//!   loop.** `maintenance::PRESSURE_INTERVAL` (5 min) exists to bound how
//!   often an expensive disk/cache sweep runs, and piggybacking an RSS check
//!   on it would either wait up to 5 minutes to notice a spike (useless for
//!   a test that must fit a nextest budget) or force every pressure tick to
//!   fire needlessly often, dragging real cache-maintenance work along with
//!   it. [`RSS_SAMPLE_INTERVAL`] is a separate, independent task so tightening
//!   it costs nothing but the read itself — `/proc/<pid>/status` on Linux —
//!   and never perturbs `maintenance::run_loop`'s own scheduling.
//! - **A file the test asserts on, not a daemon exit.** Three options exist
//!   for turning a breach into a test failure: (1) record it in a file the
//!   test reads, (2) exit the daemon non-zero, (3) have the test sample RSS
//!   itself from outside. (2) was rejected first: a daemon that kills itself
//!   mid-build fails the *build*, and the test would see a `connection
//!   refused`/broker-retry storm that never names the daemon, the ceiling, or
//!   the observed RSS — exactly the illegible failure the task calls out.
//!   (3) is a real alternative and a good backstop, but sampling from
//!   outside the process races the peak: an external poller can miss an
//!   instantaneous spike between samples in a way an in-process sampler
//!   (this module) that shares the sampling loop with a mimalloc read
//!   cannot. So this module owns the peak, and the reproduction test in
//!   `crates/soldr-cli/tests/daemon/` also reads the same file — the
//!   daemon's own account of itself, not a race against it.
//! - **Never exit, never refuse work.** A breach is recorded and logged
//!   (`tracing::warn!`, event `daemon_rss_ceiling_breached`) but the
//!   watchdog keeps sampling. The daemon exists to serve builds; a
//!   diagnostic ceiling is not a reason to stop serving them.

use crate::core::SoldrPaths;
use crate::daemon::maintenance::ShutdownSignal;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `SOLDR_DAEMON_RSS_CEILING_BYTES` — optional. When unset (the default),
/// [`ceiling_bytes_from_env`] returns `None` and no watchdog task is
/// spawned at all. Set to a positive byte count to make the daemon record
/// (never enforce — see module docs) a breach of that ceiling. Documented
/// in `docs/API.md`'s Environment Variables table.
pub const RSS_CEILING_ENV_VAR: &str = "SOLDR_DAEMON_RSS_CEILING_BYTES";

/// Sampling cadence for the watchdog task. Only ever runs when a ceiling is
/// configured (see module docs for why this does not reuse
/// `maintenance::PRESSURE_INTERVAL`). Two seconds is short enough that a
/// nextest-budget-friendly reproduction test (tens of seconds of build
/// workload) still collects a double-digit number of samples, and long
/// enough that the read itself (a few microseconds of `/proc` parsing) is
/// nowhere near a measurable tax on the daemon.
pub const RSS_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

const SCHEMA_VERSION: u32 = 1;

/// Read [`RSS_CEILING_ENV_VAR`]. `None` for unset, empty, non-numeric, or
/// zero — a `0` ceiling would breach on the very first sample for no
/// diagnostic value, so it is treated the same as unset rather than as a
/// (useless) always-breached configuration.
pub fn ceiling_bytes_from_env() -> Option<u64> {
    parse_ceiling_bytes(std::env::var(RSS_CEILING_ENV_VAR).ok().as_deref())
}

/// Pure parser split out of [`ceiling_bytes_from_env`] so the parsing rules
/// are unit-testable without mutating process-global environment state
/// (which would need `soldr_cli::TEST_PROCESS_ENV_LOCK`, unavailable to
/// this crate, and unnecessary for a pure function).
fn parse_ceiling_bytes(raw: Option<&str>) -> Option<u64> {
    raw?.trim().parse::<u64>().ok().filter(|bytes| *bytes > 0)
}

/// Where the watchdog publishes its status: a sibling of
/// `maintenance::status_path`'s `maintenance-status-v1.json`, but a
/// separate file and schema so this diagnostic never perturbs the
/// unrelated cache-maintenance status contract other tooling already
/// reads.
pub fn status_path(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join("soldr-daemon").join("rss-ceiling-v1.json")
}

/// Read the current watchdog status, if the watchdog has written one yet.
pub fn read_status(paths: &SoldrPaths) -> Option<RssCeilingStatus> {
    let body = std::fs::read_to_string(status_path(paths)).ok()?;
    serde_json::from_str(&body).ok()
}

/// One sample's worth of legible state: which daemon, what ceiling, what was
/// observed, and — because the whole point of soldr#3038 is telling live
/// data from allocator retention — mimalloc's own exact counters alongside
/// the OS-reported RSS.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RssCeilingStatus {
    pub schema_version: u32,
    /// PID of the daemon process this status describes. A test that spawned
    /// the daemon itself already knows this PID from `Child::id()`; carrying
    /// it here too makes the file self-describing for a human reading it
    /// out of band (e.g. after a CI failure).
    pub pid: u32,
    pub ceiling_bytes: u64,
    pub last_rss_bytes: u64,
    pub peak_rss_bytes: u64,
    pub sample_count: u64,
    pub breached: bool,
    pub first_breach_at_ms: Option<i64>,
    /// `mimalloc_pprof::prof::stats().heap.committed` at the last sample:
    /// bytes mimalloc currently holds committed from the OS. Compares
    /// directly against `last_rss_bytes` — a daemon where these two track
    /// each other closely is holding live data; one where RSS keeps growing
    /// while `heap.committed` does not points at something else entirely
    /// (a leak outside the allocator, or fragmentation the allocator itself
    /// cannot see).
    pub mimalloc_heap_committed_bytes: Option<u64>,
    /// Whether the linked mimalloc build tracks `heap.malloc_requested`
    /// (`MI_STAT >= 2`). The `mimalloc-pprof` crate exposes no Cargo feature
    /// for this (verified against its `Cargo.toml`/`build.rs`), so a default
    /// release build always reports `false` here; recorded anyway so this
    /// field's absence is never silently mistaken for "the allocator
    /// requested zero bytes".
    pub mimalloc_stats_detailed: bool,
    pub updated_at_ms: i64,
}

/// Run the watchdog until `shutdown` fires. Spawned only when
/// [`ceiling_bytes_from_env`] returned `Some` — see `server_runtime.rs`.
///
/// A tick that cannot read this process's own RSS (platform/host resource
/// reader returned `None`) is skipped rather than treated as a zero
/// reading: see `soldr_platform::host::resources::process_rss_bytes`'s own
/// contract for why "unreadable" and "zero" must never be conflated.
pub async fn run_watchdog(paths: SoldrPaths, shutdown: Arc<ShutdownSignal>, ceiling_bytes: u64) {
    let pid = std::process::id();
    let mut status = RssCeilingStatus {
        schema_version: SCHEMA_VERSION,
        pid,
        ceiling_bytes,
        ..Default::default()
    };
    loop {
        tokio::select! {
            _ = shutdown.wait() => return,
            _ = tokio::time::sleep(RSS_SAMPLE_INTERVAL) => {}
        }
        if shutdown.is_requested() {
            return;
        }
        let Some(rss) = crate::platform::host::resources::process_rss_bytes(pid) else {
            continue;
        };
        status.sample_count += 1;
        status.last_rss_bytes = rss;
        status.peak_rss_bytes = status.peak_rss_bytes.max(rss);
        if rss > ceiling_bytes && !status.breached {
            status.breached = true;
            status.first_breach_at_ms = Some(unix_millis());
            // Loud on purpose, and the daemon keeps running: see module
            // docs for why this never exits or refuses work.
            tracing::warn!(
                event = "daemon_rss_ceiling_breached",
                pid,
                ceiling_bytes,
                rss_bytes = rss,
                "soldr-daemon RSS exceeded SOLDR_DAEMON_RSS_CEILING_BYTES (soldr#3038); \
                 continuing to serve builds rather than exiting"
            );
        }
        let mimalloc = mimalloc_pprof::prof::stats();
        status.mimalloc_heap_committed_bytes = Some(mimalloc.heap.committed as u64);
        status.mimalloc_stats_detailed = mimalloc.heap.detailed;
        status.updated_at_ms = unix_millis();
        let _ = write_status(&paths, &status);
    }
}

/// Same temp-then-rename shape as `maintenance::atomic_write`, duplicated
/// rather than shared: that helper is private to `maintenance.rs`, and this
/// module intentionally has no other dependency on the maintenance status
/// contract (see module docs — separate file, separate schema).
fn write_status(paths: &SoldrPaths, status: &RssCeilingStatus) -> std::io::Result<()> {
    let path = status_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(status).map_err(std::io::Error::other)?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, &body)?;
    if let Err(error) = std::fs::rename(&temp, &path) {
        if path.exists() {
            std::fs::remove_file(&path)?;
            std::fs::rename(&temp, &path)?;
        } else {
            return Err(error);
        }
    }
    Ok(())
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceiling_parser_accepts_only_positive_integers() {
        assert_eq!(parse_ceiling_bytes(Some("536870912")), Some(536_870_912));
        assert_eq!(parse_ceiling_bytes(Some("  1024  ")), Some(1024));
    }

    #[test]
    fn ceiling_parser_rejects_unset_zero_and_garbage() {
        assert_eq!(
            parse_ceiling_bytes(None),
            None,
            "unset must mean no ceiling"
        );
        assert_eq!(
            parse_ceiling_bytes(Some("0")),
            None,
            "a 0 ceiling is not a useful ceiling"
        );
        assert_eq!(parse_ceiling_bytes(Some("")), None);
        assert_eq!(parse_ceiling_bytes(Some("not-a-number")), None);
        assert_eq!(
            parse_ceiling_bytes(Some("-5")),
            None,
            "negative bytes cannot parse as u64"
        );
    }

    #[test]
    fn status_round_trips_through_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(dir.path().to_path_buf());
        assert!(read_status(&paths).is_none(), "no status written yet");

        let status = RssCeilingStatus {
            schema_version: SCHEMA_VERSION,
            pid: 4242,
            ceiling_bytes: 512 << 20,
            last_rss_bytes: 100 << 20,
            peak_rss_bytes: 150 << 20,
            sample_count: 7,
            breached: false,
            first_breach_at_ms: None,
            mimalloc_heap_committed_bytes: Some(90 << 20),
            mimalloc_stats_detailed: false,
            updated_at_ms: 1_700_000_000_000,
        };
        write_status(&paths, &status).expect("write status");
        assert_eq!(read_status(&paths), Some(status));
    }

    /// An end-to-end pass of the real async loop: a 1-byte ceiling breaches
    /// on the very first readable sample (any live process holds more than
    /// one byte of RSS), so this proves the loop samples, detects the
    /// breach, attaches mimalloc counters, and writes a status file a test
    /// can assert on -- without spawning a real daemon process.
    #[tokio::test]
    async fn watchdog_records_a_breach_and_mimalloc_counters() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(dir.path().to_path_buf());
        let shutdown = Arc::new(ShutdownSignal::default());

        let stop_shutdown = Arc::clone(&shutdown);
        let stopper = tokio::spawn(async move {
            // One sample at RSS_SAMPLE_INTERVAL, plus slack for the write.
            tokio::time::sleep(RSS_SAMPLE_INTERVAL * 3).await;
            stop_shutdown.request();
        });

        run_watchdog(paths.clone(), Arc::clone(&shutdown), 1).await;
        stopper.await.expect("stopper task");

        let status = read_status(&paths).expect("watchdog must have written a status");
        assert!(
            status.sample_count > 0,
            "watchdog must have sampled at least once"
        );
        assert!(status.breached, "a 1-byte ceiling must breach: {status:?}");
        assert!(status.first_breach_at_ms.is_some());
        assert!(
            status.peak_rss_bytes > 1,
            "peak RSS must exceed the 1-byte ceiling: {status:?}"
        );
        assert!(
            status.mimalloc_heap_committed_bytes.is_some(),
            "mimalloc counters must be attached even though the sampled profiler \
             was never started: {status:?}"
        );
    }
}
