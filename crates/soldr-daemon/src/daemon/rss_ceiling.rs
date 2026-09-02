//! Optional daemon/broker RSS ceiling, fail-fast (soldr#3038 / soldr#3057).
//!
//! A canonical `soldr-daemon` reached 11.7 GiB of private anonymous memory
//! on production hardware, and a follow-up host audit found the leak is
//! two-tier: 118 `soldr-daemon` processes holding 14.57 GiB (one at 12.4
//! GiB after 43 hours) *and* 180 `soldr-broker` processes holding 3.61 GiB.
//! This module is the opt-in watchdog that samples a process's own resident
//! set on a short cadence and, on breach, writes a legible memory dump and
//! terminates the process immediately.
//!
//! ## This module used to "never exit" -- that decision was reversed
//!
//! The original soldr#3038 landing recorded a breach in a status file and
//! kept the daemon running, reasoning that an in-flight-build-killing exit
//! would surface as an illegible `connection refused` naming neither the
//! daemon, the ceiling, nor the observed RSS. That tradeoff was overruled:
//! a daemon (or broker) that is already outside its memory budget is not
//! trustworthy to keep serving builds, and staying up let the two-tier leak
//! above accumulate for 43 hours undetected. The fix for illegibility is not
//! "never exit" -- it is "dump first, then exit, and teach the caller to
//! check for the dump before blaming the transport". See
//! `crates/soldr-cli/tests/daemon/daemon_rss_ceiling.rs` for the caller side
//! of that contract.
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
//!   configured (see `server_runtime.rs` for the daemon, `broker_server.rs`
//!   for the broker), so an ordinary run pays no extra timer, no extra file
//!   write, no extra `mimalloc_pprof::prof::stats()` call, and — see below —
//!   no sampled-profiler overhead either.
//! - **A dedicated, fast cadence — not the existing 5-minute maintenance
//!   loop.** `maintenance::PRESSURE_INTERVAL` (5 min) exists to bound how
//!   often an expensive disk/cache sweep runs, and piggybacking an RSS check
//!   on it would either wait up to 5 minutes to notice a spike (useless for
//!   a test that must fit a nextest budget) or force every pressure tick to
//!   fire needlessly often, dragging real cache-maintenance work along with
//!   it. [`RSS_SAMPLE_INTERVAL`] is a separate, independent task so tightening
//!   it costs nothing but the read itself — `/proc/<pid>/status` on Linux —
//!   and never perturbs `maintenance::run_loop`'s own scheduling.
//! - **Dump first, then die — never a graceful drain.** A breach means the
//!   process is already over budget; attempting the ordinary graceful
//!   shutdown sequence (`shutdown_phase("shutdown-phase-maintenance")` and
//!   friends in `server_runtime.rs`) would run more code, allocate more, and
//!   delay the exit the ceiling exists to force. [`die_on_breach`] calls
//!   `std::process::exit` directly, bypassing `ShutdownSignal` entirely.
//! - **The sampled profiler is opt-in at runtime, gated on the same env
//!   var.** `mimalloc_pprof::prof::dump_file` only produces a useful heap
//!   profile if `prof::start` was called first. Starting it unconditionally
//!   would tax every ordinary build with sampling overhead for a dump that
//!   is thrown away 99.9% of the time, so [`start_sampled_profiler_if_configured`]
//!   is called once at process startup and only starts sampling when
//!   [`ceiling_bytes_from_env`] returned `Some` — mirroring the watchdog
//!   task's own gating. The *exact* counters (`prof::stats().heap`) are
//!   always available regardless — see `soldr-cli/Cargo.toml`'s
//!   `mimalloc-pprof` dependency comment for why the `pprof` build feature
//!   stays unconditionally on.
//! - **The broker watches its own RSS, not the daemon's.** See
//!   `broker_server.rs`'s call site and its comment for the full rationale:
//!   in short, the second finding above shows the broker leaks
//!   independently of any one daemon, self-monitoring needs no new
//!   cross-process plumbing (the broker's own `Child`/wait() story is
//!   already broken per that finding, so anything relying on it would
//!   inherit the same gap), and it reuses this exact, already-tested
//!   sample/dump/die primitive unmodified.

use crate::core::SoldrPaths;
use crate::daemon::maintenance::ShutdownSignal;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `SOLDR_DAEMON_RSS_CEILING_BYTES` — optional. When unset (the default),
/// [`ceiling_bytes_from_env`] returns `None` and no watchdog task is
/// spawned at all, in either the daemon or the broker. Set to a positive
/// byte count to make the process dump its own memory state and exit
/// non-zero the first time its RSS exceeds this value. Documented in
/// `docs/API.md`'s Environment Variables table.
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
const BREACH_SCHEMA_VERSION: u32 = 1;

/// Which long-lived soldr process is reporting a sample or a breach. Carried
/// end to end — through the status file, the breach dump, and the legible
/// failure message a test constructs — so a human (or CI log) never has to
/// guess whether `pid 4242` was the daemon or its broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessRole {
    Daemon,
    Broker,
}

impl std::fmt::Display for ProcessRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ProcessRole::Daemon => "daemon",
            ProcessRole::Broker => "broker",
        })
    }
}

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

/// Sample interval (bytes between samples) used by
/// [`start_sampled_profiler_if_configured`], deliberately finer than
/// mimalloc-pprof's own ~512 KiB default (`prof::start(0)`).
///
/// A process that breaches a tight ceiling can do so *fast* -- the whole
/// point of a small ceiling is a breach within the first watchdog tick,
/// `RSS_SAMPLE_INTERVAL` (2s) after startup. At the crate's default
/// interval, a broker or daemon that has not yet allocated a cumulative
/// 512 KiB through the sampled hook when it breaches produces a `heap.pprof`
/// with zero samples -- a symbolizable-in-principle but empty dump, useless
/// for the actual question ("what was live"). Observed directly while
/// building this module's reproduction: a `soldr-broker` breach with the
/// crate default recorded `heap profile: 0: 0 [0: 0]` -- no samples at all,
/// despite `MAPPED_LIBRARIES` correctly naming the binary. 16 KiB is dense
/// enough that even a process breaching within a couple of seconds of
/// startup has almost certainly crossed several sample boundaries, while
/// staying far short of "sample every allocation" (which would make the
/// stack-walk cost the watchdog is trying to observe rather than add to).
/// The cost is paid only when a ceiling is configured at all -- see the
/// module docs' "opt-in at runtime" design decision.
pub const HEAP_PROFILE_SAMPLE_INTERVAL_BYTES: usize = 16 * 1024;

/// Start mimalloc-pprof's *sampled* heap profiler, but only when a ceiling
/// is actually configured. Call exactly once, at process startup, before
/// spawning the watchdog task -- see module docs for why this must be
/// opt-in rather than unconditional. A daemon/broker that never sees this
/// env var pays nothing: no sampling, no allocation hook overhead.
///
/// Returns whether sampling was requested at all (i.e. `ceiling_bytes` was
/// `Some`), for the caller's own startup log line. Failure to actually
/// start the profiler (mimalloc reports it was already running, or the
/// build lacks the `pprof` feature) is logged here and does not stop
/// startup -- a breach dump with an empty `heap.pprof` is still useful for
/// its exact counters and `/proc` snapshot.
pub fn start_sampled_profiler_if_configured(ceiling_bytes: Option<u64>) -> bool {
    let Some(ceiling_bytes) = ceiling_bytes else {
        return false;
    };
    if !mimalloc_pprof::prof::start(HEAP_PROFILE_SAMPLE_INTERVAL_BYTES) {
        tracing::warn!(
            ceiling_bytes,
            "SOLDR_DAEMON_RSS_CEILING_BYTES is set but mimalloc-pprof's sampled profiler \
             could not be started; a breach dump's heap.pprof will be empty, but exact \
             allocator counters and /proc snapshots remain available"
        );
    }
    true
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
    /// PID of the process this status describes. A test that spawned the
    /// process itself already knows this PID from `Child::id()`; carrying
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

/// Full account of a breach: where the dump landed and what each artifact
/// inside it is, plus the fields a legible failure message is built from.
/// Serialized to `summary.json` inside the dump directory, and returned to
/// the caller so `die_on_breach` can log the same facts before exiting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreachSummary {
    pub schema_version: u32,
    pub pid: u32,
    pub role: ProcessRole,
    pub ceiling_bytes: u64,
    pub last_rss_bytes: u64,
    pub peak_rss_bytes: u64,
    pub created_at_ms: i64,
    pub dump_dir: PathBuf,
    /// `Some` iff `mimalloc_pprof::prof::dump_file` succeeded.
    pub heap_profile_path: Option<PathBuf>,
    /// `Some` iff the dump call failed -- e.g. the sampled profiler was
    /// never started (see [`start_sampled_profiler_if_configured`]).
    pub heap_profile_error: Option<String>,
    pub mimalloc_stats_path: PathBuf,
    /// `/proc/self/status`, copied verbatim. `None` on platforms with no
    /// `/proc` (macOS, Windows) -- the summary still names the gap rather
    /// than silently omitting the field.
    pub proc_status_path: Option<PathBuf>,
    /// `/proc/self/smaps_rollup`, copied verbatim. Same platform caveat as
    /// `proc_status_path`.
    pub proc_smaps_rollup_path: Option<PathBuf>,
}

/// Directory a breach dump for `pid` at `created_at_ms` is written to.
/// Includes the pid (not just the timestamp) so a daemon and its broker —
/// which share the same `paths.cache` when the broker is watching its own
/// distinct, broker-owned root — can never collide on the same directory
/// even if both breach within the same millisecond.
fn breach_dir(paths: &SoldrPaths, created_at_ms: i64, pid: u32) -> PathBuf {
    paths
        .cache
        .join("soldr-daemon")
        .join(format!("memory-breach-{created_at_ms}-{pid}"))
}

/// Write every breach artifact required by soldr#3057: the pprof-compatible
/// sampled heap profile, mimalloc's exact counters, the platform memory
/// snapshot, and a JSON summary tying them together. Best-effort per
/// artifact -- a failure on one (e.g. the sampled profiler was never
/// started) does not prevent the others from being written, because a
/// partial dump is still far more legible than none.
pub fn write_breach_dump(
    paths: &SoldrPaths,
    role: ProcessRole,
    pid: u32,
    ceiling_bytes: u64,
    last_rss_bytes: u64,
    peak_rss_bytes: u64,
) -> std::io::Result<BreachSummary> {
    let created_at_ms = unix_millis();
    let dir = breach_dir(paths, created_at_ms, pid);
    std::fs::create_dir_all(&dir)?;

    let heap_profile_dest = dir.join("heap.pprof");
    let (heap_profile_path, heap_profile_error) =
        match mimalloc_pprof::prof::dump_file(&heap_profile_dest) {
            Ok(()) => (Some(heap_profile_dest), None),
            Err(error) => (None, Some(error.to_string())),
        };

    let mimalloc_stats_path = dir.join("mimalloc-stats.json");
    let _ = std::fs::write(
        &mimalloc_stats_path,
        serde_json::to_vec_pretty(&exact_counters_json()).unwrap_or_default(),
    );

    let proc_status_path =
        copy_proc_snapshot("/proc/self/status", &dir.join("proc-self-status.txt"));
    let proc_smaps_rollup_path = copy_proc_snapshot(
        "/proc/self/smaps_rollup",
        &dir.join("proc-self-smaps_rollup.txt"),
    );

    let summary = BreachSummary {
        schema_version: BREACH_SCHEMA_VERSION,
        pid,
        role,
        ceiling_bytes,
        last_rss_bytes,
        peak_rss_bytes,
        created_at_ms,
        dump_dir: dir.clone(),
        heap_profile_path,
        heap_profile_error,
        mimalloc_stats_path,
        proc_status_path,
        proc_smaps_rollup_path,
    };
    let summary_json = serde_json::to_vec_pretty(&summary).map_err(std::io::Error::other)?;
    std::fs::write(dir.join("summary.json"), summary_json)?;
    Ok(summary)
}

/// `mimalloc_pprof::prof::stats()`, flattened into a plain JSON object.
/// Kept separate from [`BreachSummary`] (rather than embedding `ProfStats`
/// directly) because `ProfStats`/`HeapStats` derive neither `Serialize` nor
/// `Deserialize` upstream.
fn exact_counters_json() -> serde_json::Value {
    let stats = mimalloc_pprof::prof::stats();
    serde_json::json!({
        "enabled": stats.enabled,
        "accum": stats.accum,
        "sample_rate": stats.sample_rate,
        "live_samples": stats.live_samples,
        "live_bytes": stats.live_bytes,
        "accum_samples": stats.accum_samples,
        "accum_bytes": stats.accum_bytes,
        "unique_stacks": stats.unique_stacks,
        "arena_committed": stats.arena_committed,
        "stack_table_overflows": stats.stack_table_overflows,
        "dropped_samples": stats.dropped_samples,
        "heap": {
            "committed": stats.heap.committed,
            "reserved": stats.heap.reserved,
            "malloc_requested": stats.heap.malloc_requested,
            "pages": stats.heap.pages,
            "pages_abandoned": stats.heap.pages_abandoned,
            "heaps": stats.heap.heaps,
            "theaps": stats.heap.theaps,
            "purged": stats.heap.purged,
            "detailed": stats.heap.detailed,
        },
    })
}

/// Copy one `/proc/self/*` file into the dump directory verbatim. `None` on
/// any failure (missing file, no `/proc`, permission) -- never fatal to the
/// rest of the dump. Effectively Linux-only: macOS and Windows have no
/// `/proc`, so the copy fails there and yields `None`. This is a runtime
/// probe rather than a `#[cfg(target_os)]` on purpose -- host-platform
/// selection belongs to soldr-platform (soldr#2493), and there is no
/// single-file equivalent worth a platform module here (Activity
/// Monitor-style sampling would need its own, tracked as a gap in the
/// breach summary itself via `proc_status_path: None` rather than silently
/// pretended away).
fn copy_proc_snapshot(src: &str, dest: &Path) -> Option<PathBuf> {
    if !Path::new(src).is_file() {
        return None;
    }
    std::fs::copy(src, dest).ok().map(|_| dest.to_path_buf())
}

/// The message a human (or a test's panic!) should show for a breach: names
/// the role, the pid, the ceiling, the observed peak, and where the dump
/// landed. This is the string `crates/soldr-cli/tests/daemon/daemon_rss_ceiling.rs`
/// asserts a breaching build reports *instead of* a transport error --
/// deliberately built from [`BreachSummary`] so the production exit path and
/// the test assertion describe the same facts.
pub fn legible_breach_message(summary: &BreachSummary) -> String {
    format!(
        "soldr {role} (pid {pid}) breached its {ceiling_mib:.1} MiB RSS ceiling \
         ({env_var}): observed peak {peak_mib:.1} MiB (last sample {last_mib:.1} MiB). \
         Memory dump written to {dump_dir} (heap.pprof, mimalloc-stats.json, \
         proc-self-status.txt, summary.json).",
        role = summary.role,
        pid = summary.pid,
        ceiling_mib = summary.ceiling_bytes as f64 / (1024.0 * 1024.0),
        env_var = RSS_CEILING_ENV_VAR,
        peak_mib = summary.peak_rss_bytes as f64 / (1024.0 * 1024.0),
        last_mib = summary.last_rss_bytes as f64 / (1024.0 * 1024.0),
        dump_dir = summary.dump_dir.display(),
    )
}

/// Write the breach dump and terminate the process. Never returns.
///
/// Deliberately `std::process::exit`, not `state.shutdown.request()`: a
/// breach means the process is already over its memory budget, and running
/// the ordinary graceful-drain sequence (maintenance / event-batcher /
/// compile-service, each of which allocates) is exactly the wrong thing to
/// do first. The dump is the artifact that matters; nothing else does.
///
/// Split out of [`run_watchdog`]/[`run_watchdog_notify`] so the substantial
/// logic -- [`write_breach_dump`] -- stays unit-testable without an
/// `std::process::exit` call tearing down the test process along with it.
fn die_on_breach(
    paths: &SoldrPaths,
    role: ProcessRole,
    pid: u32,
    ceiling_bytes: u64,
    last_rss_bytes: u64,
    peak_rss_bytes: u64,
) -> ! {
    match write_breach_dump(
        paths,
        role,
        pid,
        ceiling_bytes,
        last_rss_bytes,
        peak_rss_bytes,
    ) {
        Ok(summary) => {
            tracing::error!(
                event = "daemon_rss_ceiling_breach_dump_written",
                role = %role,
                pid,
                dump_dir = %summary.dump_dir.display(),
                "{}",
                legible_breach_message(&summary)
            );
        }
        Err(error) => {
            tracing::error!(
                event = "daemon_rss_ceiling_breach_dump_failed",
                role = %role,
                pid,
                ceiling_bytes,
                last_rss_bytes,
                peak_rss_bytes,
                %error,
                "soldr {role} (pid {pid}) breached its RSS ceiling but the memory dump \
                 itself could not be written; exiting anyway"
            );
        }
    }
    std::process::exit(1);
}

/// Outcome of one sampling tick, returned by [`sample_tick`] so the actual
/// process-killing decision stays out of that pure(-ish) function and is
/// independently testable.
enum Tick {
    /// RSS was unreadable this tick (process gone, no `/proc`); skip.
    Unreadable,
    /// Sampled successfully, still under the ceiling (or already breached
    /// on an earlier tick -- see `run_watchdog`'s `!status.breached` guard).
    Continue,
    /// This tick is the first to observe RSS over `ceiling_bytes`.
    Breach { rss_bytes: u64, peak_rss_bytes: u64 },
}

/// Sample `pid`'s RSS once, fold it into `status`, and report whether this
/// tick is a (new) breach. Pure aside from the one `/proc` read and the
/// mimalloc stats call -- no I/O to `paths`, no process exit -- so it is
/// exercised directly by `sample_tick_detects_a_breach_and_attaches_mimalloc_counters`
/// without needing `std::process::exit` anywhere near the test process.
fn sample_tick(pid: u32, ceiling_bytes: u64, status: &mut RssCeilingStatus) -> Tick {
    let Some(rss) = crate::platform::host::resources::process_rss_bytes(pid) else {
        return Tick::Unreadable;
    };
    status.sample_count += 1;
    status.last_rss_bytes = rss;
    status.peak_rss_bytes = status.peak_rss_bytes.max(rss);
    let mimalloc = mimalloc_pprof::prof::stats();
    status.mimalloc_heap_committed_bytes = Some(mimalloc.heap.committed as u64);
    status.mimalloc_stats_detailed = mimalloc.heap.detailed;
    status.updated_at_ms = unix_millis();
    if rss > ceiling_bytes && !status.breached {
        status.breached = true;
        status.first_breach_at_ms = Some(unix_millis());
        Tick::Breach {
            rss_bytes: rss,
            peak_rss_bytes: status.peak_rss_bytes,
        }
    } else {
        Tick::Continue
    }
}

/// Run the watchdog until `shutdown` fires OR a breach is observed, in which
/// case this dumps memory and calls `std::process::exit` -- it does not
/// return in that case. Spawned only when [`ceiling_bytes_from_env`]
/// returned `Some` -- see `server_runtime.rs`.
///
/// A tick that cannot read this process's own RSS (platform/host resource
/// reader returned `None`) is skipped rather than treated as a zero
/// reading: see `soldr_platform::host::resources::process_rss_bytes`'s own
/// contract for why "unreadable" and "zero" must never be conflated.
pub async fn run_watchdog(
    paths: SoldrPaths,
    shutdown: Arc<ShutdownSignal>,
    ceiling_bytes: u64,
    role: ProcessRole,
) {
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
        match sample_tick(pid, ceiling_bytes, &mut status) {
            Tick::Unreadable => continue,
            Tick::Continue => {
                let _ = write_status(&paths, &status);
            }
            Tick::Breach {
                rss_bytes,
                peak_rss_bytes,
            } => {
                let _ = write_status(&paths, &status);
                die_on_breach(&paths, role, pid, ceiling_bytes, rss_bytes, peak_rss_bytes);
            }
        }
    }
}

/// Same loop as [`run_watchdog`], for a caller whose shutdown signal is a
/// bare `tokio::sync::Notify` rather than [`ShutdownSignal`] -- the shape
/// `broker_server.rs`'s `serve_loop` already uses for its own
/// `run_route_reaper` task. Duplicated rather than made generic over the
/// shutdown type: the two shutdown primitives are not part of the same
/// trait anywhere in this codebase, and the loop body is fifteen lines.
pub async fn run_watchdog_notify(
    paths: SoldrPaths,
    shutdown: Arc<tokio::sync::Notify>,
    ceiling_bytes: u64,
    role: ProcessRole,
) {
    let pid = std::process::id();
    let mut status = RssCeilingStatus {
        schema_version: SCHEMA_VERSION,
        pid,
        ceiling_bytes,
        ..Default::default()
    };
    loop {
        tokio::select! {
            _ = shutdown.notified() => return,
            _ = tokio::time::sleep(RSS_SAMPLE_INTERVAL) => {}
        }
        match sample_tick(pid, ceiling_bytes, &mut status) {
            Tick::Unreadable => continue,
            Tick::Continue => {
                let _ = write_status(&paths, &status);
            }
            Tick::Breach {
                rss_bytes,
                peak_rss_bytes,
            } => {
                let _ = write_status(&paths, &status);
                die_on_breach(&paths, role, pid, ceiling_bytes, rss_bytes, peak_rss_bytes);
            }
        }
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

    /// Exercises the real sampling logic end to end -- detection, mimalloc
    /// counter attachment, and the one-shot `!status.breached` guard --
    /// without ever reaching `die_on_breach`'s `std::process::exit`. A
    /// 1-byte ceiling breaches on the very first readable sample (any live
    /// process holds more than one byte of RSS).
    #[test]
    fn sample_tick_detects_a_breach_and_attaches_mimalloc_counters() {
        let pid = std::process::id();
        let mut status = RssCeilingStatus {
            schema_version: SCHEMA_VERSION,
            pid,
            ceiling_bytes: 1,
            ..Default::default()
        };

        let first = sample_tick(pid, 1, &mut status);
        assert!(
            matches!(first, Tick::Breach { .. }),
            "a 1-byte ceiling must breach on the first readable sample"
        );
        assert!(status.breached);
        assert!(status.first_breach_at_ms.is_some());
        assert!(status.sample_count >= 1);
        assert!(
            status.peak_rss_bytes > 1,
            "peak RSS must exceed the 1-byte ceiling: {status:?}"
        );
        assert!(
            status.mimalloc_heap_committed_bytes.is_some(),
            "mimalloc counters must be attached even though the sampled profiler \
             was never started: {status:?}"
        );

        // The second tick must not re-report a breach: `!status.breached`
        // in `run_watchdog`/`run_watchdog_notify` guards `die_on_breach`
        // from being invoked twice for the same watchdog's lifetime.
        let second = sample_tick(pid, 1, &mut status);
        assert!(
            matches!(second, Tick::Continue),
            "a second tick past the first breach must not report Breach again"
        );
    }

    #[test]
    fn sample_tick_skips_an_unreadable_pid_without_touching_status() {
        // PID 0 is never a real process on Linux/macOS, so the platform RSS
        // reader must return `None` rather than a bogus zero reading.
        let mut status = RssCeilingStatus {
            schema_version: SCHEMA_VERSION,
            pid: 0,
            ceiling_bytes: 1,
            ..Default::default()
        };
        let tick = sample_tick(0, 1, &mut status);
        assert!(matches!(tick, Tick::Unreadable));
        assert_eq!(status.sample_count, 0, "an unreadable tick must not count");
    }

    /// `write_breach_dump` is the artifact soldr#3057 is actually about:
    /// this proves every file it promises exists and is non-empty JSON
    /// where JSON is promised, without going anywhere near
    /// `std::process::exit`.
    #[test]
    fn write_breach_dump_produces_every_promised_artifact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(dir.path().to_path_buf());
        let pid = std::process::id();

        let summary = write_breach_dump(
            &paths,
            ProcessRole::Daemon,
            pid,
            512 << 20,
            600 << 20,
            700 << 20,
        )
        .expect("write_breach_dump must succeed under a writable tempdir");

        assert_eq!(summary.pid, pid);
        assert_eq!(summary.role, ProcessRole::Daemon);
        assert!(summary
            .dump_dir
            .starts_with(paths.cache.join("soldr-daemon")));
        assert!(summary.dump_dir.is_dir(), "dump dir must exist on disk");

        // mimalloc-stats.json and summary.json are unconditional.
        let stats_body = std::fs::read_to_string(&summary.mimalloc_stats_path)
            .expect("read mimalloc-stats.json");
        let _: serde_json::Value =
            serde_json::from_str(&stats_body).expect("mimalloc-stats.json must be valid JSON");

        let summary_body = std::fs::read_to_string(summary.dump_dir.join("summary.json"))
            .expect("read summary.json");
        let round_tripped: BreachSummary =
            serde_json::from_str(&summary_body).expect("summary.json must round-trip");
        assert_eq!(round_tripped.pid, pid);
        assert_eq!(round_tripped.ceiling_bytes, 512 << 20);
        assert_eq!(round_tripped.peak_rss_bytes, 700 << 20);

        // /proc is Linux-only; assert the field is populated where the
        // source exists and explicitly absent (not merely unwritten)
        // everywhere else. Probed at runtime, not via `#[cfg(target_os)]`,
        // for the same soldr#2493 boundary reason as `copy_proc_snapshot`.
        if Path::new("/proc/self/status").is_file() {
            let status_path = summary.proc_status_path.expect("/proc/self/status copy");
            assert!(status_path.is_file());
        } else {
            assert!(summary.proc_status_path.is_none());
        }
        if Path::new("/proc/self/smaps_rollup").is_file() {
            let smaps_path = summary
                .proc_smaps_rollup_path
                .expect("/proc/self/smaps_rollup copy");
            assert!(smaps_path.is_file());
        } else {
            assert!(summary.proc_smaps_rollup_path.is_none());
        }
    }

    #[test]
    fn legible_breach_message_names_role_ceiling_peak_and_dump_path() {
        let summary = BreachSummary {
            schema_version: BREACH_SCHEMA_VERSION,
            pid: 4242,
            role: ProcessRole::Broker,
            ceiling_bytes: 512 << 20,
            last_rss_bytes: 600 << 20,
            peak_rss_bytes: 700 << 20,
            created_at_ms: 1_700_000_000_000,
            dump_dir: PathBuf::from("/tmp/example/memory-breach-1700000000000-4242"),
            heap_profile_path: Some(PathBuf::from(
                "/tmp/example/memory-breach-1700000000000-4242/heap.pprof",
            )),
            heap_profile_error: None,
            mimalloc_stats_path: PathBuf::from(
                "/tmp/example/memory-breach-1700000000000-4242/mimalloc-stats.json",
            ),
            proc_status_path: None,
            proc_smaps_rollup_path: None,
        };
        let message = legible_breach_message(&summary);
        assert!(message.contains("broker"), "{message}");
        assert!(message.contains("4242"), "{message}");
        assert!(message.contains("512.0 MiB"), "{message}");
        assert!(message.contains("700.0 MiB"), "{message}");
        assert!(
            message.contains("memory-breach-1700000000000-4242"),
            "{message}"
        );
        assert!(message.contains(RSS_CEILING_ENV_VAR), "{message}");
    }
}
