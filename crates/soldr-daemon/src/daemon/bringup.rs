//! Per-phase timing for daemon cold start (soldr#3163).
//!
//! [`crate::daemon::server_runtime::run`] used to announce nothing between the
//! broker launching it and the route claim appearing. Everything that can make
//! a daemon cold start slow -- acquiring root ownership (which has its own
//! retry budget), claiming the control endpoint, resolving listeners, rotating
//! the lifecycle journal, opening the state store, and initializing the
//! embedded zccache service -- happened inside that silent gap.
//!
//! soldr#3163 measured a **~3.5 s gap between daemon launch and route claim**
//! on a cold start, with roughly forty tests paying it independently, and could
//! not attribute a single millisecond of it. Three hypotheses were proposed for
//! where it went; all three were measured and all three were wrong. The
//! conclusion was that the instrument had to come before any more guessing.
//!
//! This is that instrument, and it is deliberately the same one
//! [`soldr_cli::broker_bringup`] already gives the broker, so the two halves of
//! a cold start read the same way. Every phase is reported the moment it
//! completes, to two places:
//!
//! - **stderr**, which the broker's launcher redirects into `daemon-spawn.log`,
//!   so a production daemon nobody is watching still leaves the timings beside
//!   the spawn record that already exists; and
//! - **`daemon-bringup.jsonl`** in the same directory, machine-readable, for
//!   the CI analysis this was built to enable.
//!
//! Records are appended as each phase ends rather than buffered until the end,
//! so a daemon that hangs mid-bringup still leaves behind everything it
//! completed. **The last line written names the phase that was entered but
//! never finished** -- which is the whole point, and the property a summary
//! printed at the end cannot have.
//!
//! # Why this is not gated behind an env var
//!
//! [`soldr_cli::startup_trace`] is opt-in because it writes to a *front door's*
//! stderr, which is a user's terminal. A daemon's stderr is a log file nobody
//! reads interactively, so the same argument that makes `broker_bringup`
//! unconditional applies here: the cost is a dozen lines per process start, and
//! a cold start that has to be reproduced before it can be diagnosed is exactly
//! the failure soldr#3163 spent three rounds of measurement on.
//!
//! # Why the log is opened late
//!
//! The first two phases complete before `SoldrPaths` is resolved, and path
//! resolution is itself a candidate phase. [`BringupRecorder::resuming`] exists
//! for that: the clock starts at process entry and the file is attached once
//! there is a directory to put it in, so the early phases are still timed and
//! still reach stderr.

use std::io::Write as _;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Stable shape for `daemon-bringup.jsonl` consumers.
const SCHEMA_VERSION: u32 = 1;

/// Phase labels. Fixed strings rather than ad-hoc literals at each call site so
/// the JSONL stays greppable and a renamed phase is a compile-time change.
pub mod phase {
    /// Building the multi-thread Tokio runtime, before any async work.
    pub const TOKIO_RUNTIME: &str = "tokio_runtime";
    /// `SoldrPaths::new`, the daemon directory, and file tracing.
    pub const RESOLVE_PATHS: &str = "resolve_paths";
    /// `RootOwnershipGuard::acquire_with_grace` -- carries a retry budget, so
    /// a slow phase here means contention with a predecessor, not slow I/O.
    pub const ROOT_OWNERSHIP: &str = "root_ownership";
    /// Claiming the control endpoint (AF_UNIX socket; no-op on Windows).
    pub const CONTROL_ENDPOINT: &str = "control_endpoint";
    /// Resolving this process's daemon identity.
    pub const DAEMON_IDENTITY: &str = "daemon_identity";
    /// Resolving the broker-facing SESSION and handoff endpoints.
    pub const SESSION_LISTENER: &str = "session_listener";
    /// Rotating the lifecycle journal and recording the `spawn` event.
    pub const LIFECYCLE_JOURNAL: &str = "lifecycle_journal";
    /// Publishing the broker route claim -- the point the front door is
    /// waiting for, and the far end of soldr#3163's unattributed gap.
    pub const ROUTE_CLAIM: &str = "route_claim";
    /// Spawning the SESSION and handoff endpoint servers.
    pub const ENDPOINT_SERVERS: &str = "endpoint_servers";
    /// Opening the state store: target registry, daemon db, cook index.
    pub const STATE_STORE: &str = "state_store";
    /// Awaiting embedded zccache initialization.
    pub const COMPILE_SERVICE: &str = "compile_service";
    /// Everything constructed; the daemon is serving.
    pub const READY: &str = "ready";
}

/// Records how long each daemon cold-start phase took.
///
/// Construction never fails: if the durable log cannot be opened, timings still
/// go to stderr. Observability must not be able to break bringup.
pub struct BringupRecorder {
    started: Instant,
    phase_started: Instant,
    log: Option<std::fs::File>,
    pid: u32,
}

impl BringupRecorder {
    /// Start recording now, with no durable log yet.
    ///
    /// The daemon cannot open its log until `SoldrPaths` is resolved, and the
    /// time that resolution takes is itself a phase worth reporting. Call
    /// [`BringupRecorder::attach_log`] once a directory is known.
    pub fn new() -> Self {
        Self::resuming(Instant::now())
    }

    /// Like [`BringupRecorder::new`], but adopting a clock that started
    /// earlier -- at process entry, before the Tokio runtime was built.
    pub fn resuming(started: Instant) -> Self {
        Self {
            started,
            phase_started: started,
            log: None,
            pid: std::process::id(),
        }
    }

    /// Attach the durable log, now that there is a directory for it.
    ///
    /// Phases already reported reached stderr and are not replayed: the JSONL
    /// is a best-effort durable copy, not the primary sink, and backfilling
    /// would put records in the file out of the order they happened.
    pub fn attach_log(&mut self, log_dir: &std::path::Path) {
        self.log = open_append(&log_dir.join("daemon-bringup.jsonl"));
    }

    /// Record that `name` just finished, and start timing the next phase.
    pub fn phase(&mut self, name: &str) {
        let now = Instant::now();
        let phase_ms = now.duration_since(self.phase_started).as_millis();
        let total_ms = now.duration_since(self.started).as_millis();
        self.phase_started = now;
        eprintln!("soldr-daemon: bringup phase={name} ms={phase_ms} total_ms={total_ms}");
        self.append(&render_record(
            self.pid,
            name,
            phase_ms,
            total_ms,
            unix_millis(),
        ));
    }

    fn append(&mut self, line: &str) {
        let Some(log) = self.log.as_mut() else {
            return;
        };
        // Best effort: a full or read-only disk must not fail daemon startup.
        let _ = writeln!(log, "{line}");
        let _ = log.flush();
    }
}

impl Default for BringupRecorder {
    fn default() -> Self {
        Self::new()
    }
}

/// Open `path` for appending, creating parents. `None` on any failure -- the
/// caller keeps reporting to stderr.
fn open_append(path: &std::path::Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// Render one JSONL record. Pure so its shape is unit-testable without I/O.
fn render_record(pid: u32, phase: &str, phase_ms: u128, total_ms: u128, unix_ms: u128) -> String {
    serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "event": "daemon_bringup_phase",
        "pid": pid,
        "phase": phase,
        "phase_ms": phase_ms as u64,
        "total_ms": total_ms as u64,
        "unix_ms": unix_ms as u64,
    })
    .to_string()
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "bringup_tests.rs"]
mod tests;
