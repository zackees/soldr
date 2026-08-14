//! Per-phase timing for broker cold start (soldr#2493 follow-up).
//!
//! `soldr broker serve` used to announce two things and nothing in between:
//! `binding stable endpoint …` before any work, and `stable endpoint bound
//! at …` after all of it. Everything that can make cold start slow — securing
//! directories, building the Tokio runtime, resolving the peer policy, hashing
//! the executable image (including any wait on another process's hash lock),
//! and the bind itself — happened inside that silent gap.
//!
//! A CI failure in that gap is therefore undiagnosable: the broker is alive,
//! has printed `binding`, and will never say why it has not printed `bound`.
//! That is exactly how soldr#2493's Linux lane failed, and reading the code
//! afterwards could not distinguish the five candidate phases.
//!
//! This module closes that gap. Every phase is reported the moment it
//! completes, to two places:
//!
//! - **stderr**, so a test that captures the child's output (or a panic
//!   message that dumps it) contains the timings without needing any artifact
//!   to be uploaded; and
//! - **`broker-bringup.jsonl`** beside `broker-spawn.log`, so a production
//!   broker spawned by the front door — whose stderr nobody is reading —
//!   leaves a durable, machine-readable record.
//!
//! Records are appended as each phase ends rather than buffered until the end,
//! so a broker that hangs mid-bringup still leaves behind everything it
//! completed. The last line written names the phase that was entered but never
//! finished.

use std::io::Write as _;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Stable shape for `broker-bringup.jsonl` consumers.
const SCHEMA_VERSION: u32 = 1;

/// Phase labels. Fixed strings rather than ad-hoc literals at each call site so
/// the JSONL stays greppable and a renamed phase is a compile-time change.
pub(crate) mod phase {
    pub(crate) const SECURE_DIRECTORIES: &str = "secure_directories";
    pub(crate) const TOKIO_RUNTIME: &str = "tokio_runtime";
    pub(crate) const PEER_POLICY: &str = "peer_policy";
    pub(crate) const INSTANCE_ID: &str = "instance_id";
    pub(crate) const BROKER_STATE: &str = "broker_state";
    pub(crate) const BIND_LISTENER: &str = "bind_listener";
}

/// Records how long each broker cold-start phase took.
///
/// Construction never fails: if the durable log cannot be opened, timings
/// still go to stderr. Observability must not be able to break bringup.
pub(crate) struct BringupRecorder {
    started: Instant,
    phase_started: Instant,
    log: Option<std::fs::File>,
    pid: u32,
}

impl BringupRecorder {
    /// Start recording. `log_dir` is the broker's own directory — the same one
    /// that holds `broker-spawn.log` — so all bringup evidence sits together.
    pub(crate) fn new(log_dir: Option<&std::path::Path>) -> Self {
        Self::resuming(Instant::now(), log_dir)
    }

    /// Like [`BringupRecorder::new`], but for a caller that started timing
    /// earlier than it could safely open the log — the broker must secure its
    /// directory before anything creates files in it, yet the time that
    /// securing took is itself a phase worth reporting.
    pub(crate) fn resuming(started: Instant, log_dir: Option<&std::path::Path>) -> Self {
        Self {
            started,
            phase_started: started,
            log: log_dir
                .map(|dir| dir.join("broker-bringup.jsonl"))
                .and_then(|path| crate::broker_spawn::open_append(&path)),
            pid: std::process::id(),
        }
    }

    /// Record that `name` just finished, and start timing the next phase.
    pub(crate) fn phase(&mut self, name: &str) {
        let now = Instant::now();
        let phase_ms = now.duration_since(self.phase_started).as_millis();
        let total_ms = now.duration_since(self.started).as_millis();
        self.phase_started = now;
        eprintln!("soldr broker: bringup phase={name} ms={phase_ms} total_ms={total_ms}");
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
        // Best effort: a full or read-only disk must not fail broker startup.
        let _ = writeln!(log, "{line}");
        let _ = log.flush();
    }
}

/// Render one JSONL record. Pure so its shape is unit-testable without I/O.
fn render_record(pid: u32, phase: &str, phase_ms: u128, total_ms: u128, unix_ms: u128) -> String {
    serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "event": "broker_bringup_phase",
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
mod tests {
    use super::*;

    crate::timed_test!(record_carries_the_stable_diagnostic_fields, {
        let line = render_record(4242, phase::BIND_LISTENER, 17, 950, 1_700_000_000_000);
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["event"], "broker_bringup_phase");
        assert_eq!(parsed["pid"], 4242);
        assert_eq!(parsed["phase"], "bind_listener");
        assert_eq!(parsed["phase_ms"], 17);
        assert_eq!(parsed["total_ms"], 950);
        // One record per line: a newline inside would corrupt the JSONL.
        assert!(!line.contains('\n'));
    });

    crate::timed_test!(phases_append_one_line_each_as_they_complete, {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut recorder = BringupRecorder::new(Some(dir.path()));
        recorder.phase(phase::SECURE_DIRECTORIES);
        recorder.phase(phase::BIND_LISTENER);

        // Written eagerly, not buffered: a broker that hangs after these two
        // phases must still leave them on disk.
        let body = std::fs::read_to_string(dir.path().join("broker-bringup.jsonl"))
            .expect("bringup log written");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "{body}");
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON");
        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("valid JSON");
        assert_eq!(first["phase"], "secure_directories");
        assert_eq!(second["phase"], "bind_listener");
        assert!(
            second["total_ms"].as_u64() >= first["total_ms"].as_u64(),
            "total_ms must be cumulative"
        );
    });

    crate::timed_test!(an_unwritable_log_location_still_reports_timings, {
        // No durable log: construction must succeed and phases must not panic.
        let mut recorder = BringupRecorder::new(None);
        recorder.phase(phase::PEER_POLICY);
    });
}
