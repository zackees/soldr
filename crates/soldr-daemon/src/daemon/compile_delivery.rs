//! Durable forensics for compiles the daemon ran but never delivered
//! (soldr#1857).
//!
//! Issue #1857 is "a dispatched compile exits 1 with no diagnostics on
//! Windows". The decisive evidence, gathered by matching `-C metadata=`
//! hashes between cargo's failing command line and zccache's compile
//! journal, is that the failing units were journaled `exit_code: 0` —
//! the compile *succeeded* and the wrapper still reported failure to
//! cargo. Whatever consumes that success does so between the compile
//! future completing and the bytes landing in the wrapper's stdio, and
//! **nothing in that window left a durable trace**:
//!
//! - a mid-compile client disconnect only reached
//!   [`crate::daemon::compile_trace`], which is inert unless
//!   `SOLDR_DAEMON_TRACE` is set — off in every real build;
//! - a failure writing the reply frames returned `Err` up to a
//!   `tracing::warn!` that goes nowhere on a detached daemon.
//!
//! So the one artifact that would distinguish "rustc rejected your
//! code" from "soldr lost a compile it had already finished" did not
//! exist. This module is that artifact: an always-on, append-only JSONL
//! log of every compile whose result the daemon produced but could not
//! hand back. It is deliberately modelled on zccache's
//! `child-terminations.jsonl` (zccache#1249) — a dedicated file, one
//! event per line, countable without grepping an interleaved log.
//!
//! Output format (one JSON object per line):
//! ```jsonl
//! {"schema_version":1,"ts_ms":…,"pid":…,"event":"client_disconnected",
//!  "detail":"eof","compile_id":"c0000001","crate_name":"protox",
//!  "target_dir":"…","elapsed_ms":131700,"exit_code":null}
//! ```
//!
//! Writes are best-effort and never block or fail a compile: a
//! diagnostic that can break a build is worse than no diagnostic.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::cache_lib::soldr_daemon_dir;
use crate::core::SoldrPaths;

/// Schema version for the JSONL rows, bumped on any field removal or
/// meaning change so offline readers can refuse rows they can't parse.
const SCHEMA_VERSION: u32 = 1;

/// Append-only JSONL log of compiles the daemon completed but could not
/// deliver. Lives beside the daemon's other state rather than under the
/// client-owned `<root>/logs/` tree, because the daemon is the only
/// writer.
#[must_use]
pub fn compile_delivery_log_path(paths: &SoldrPaths) -> PathBuf {
    soldr_daemon_dir(paths)
        .join("logs")
        .join("compile-delivery.jsonl")
}

/// Why a compile result never reached the wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndeliveredKind {
    /// The IPC connection signalled disconnect while the compile was
    /// still running, so the compile future was dropped at the
    /// `select!` boundary. Carries the disconnect flavour.
    ClientDisconnected,
    /// The compile finished — exit code and output in hand — and
    /// writing the reply frames to the connection failed.
    ReplyWriteFailed,
}

impl UndeliveredKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClientDisconnected => "client_disconnected",
            Self::ReplyWriteFailed => "reply_write_failed",
        }
    }
}

/// One undelivered-compile record.
#[derive(Debug, Clone)]
pub struct Undelivered<'a> {
    pub kind: UndeliveredKind,
    /// Free-form discriminator within `kind`: the disconnect flavour
    /// (`eof`, `read_error:BrokenPipe`, `unexpected_bytes:4`) or the
    /// wire stage that failed (`stdout_chunk`, `stderr_chunk`, `done`).
    pub detail: &'a str,
    /// Per-daemon compile id, so a row can be joined against the
    /// `SOLDR_DAEMON_TRACE` phase trace when that is enabled.
    pub compile_id: &'a str,
    pub crate_name: Option<&'a str>,
    pub target_dir: Option<&'a str>,
    /// Wall-clock spent inside the compile before delivery was lost.
    pub elapsed_ms: u64,
    /// The exit code the daemon was holding, when it had one. `Some(0)`
    /// here is the exact shape #1857 reports: a successful compile that
    /// cargo saw as a failure.
    pub exit_code: Option<i32>,
}

#[derive(Serialize)]
struct Row<'a> {
    schema_version: u32,
    ts_ms: i64,
    pid: u32,
    event: &'a str,
    detail: &'a str,
    compile_id: &'a str,
    crate_name: Option<&'a str>,
    target_dir: Option<&'a str>,
    elapsed_ms: u64,
    exit_code: Option<i32>,
}

/// Append one row. Best-effort: any error (unwritable directory, full
/// disk, serialization) is dropped silently.
pub fn record(paths: &SoldrPaths, event: &Undelivered<'_>) {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let line = match serde_json::to_string(&Row {
        schema_version: SCHEMA_VERSION,
        ts_ms,
        pid: std::process::id(),
        event: event.kind.as_str(),
        detail: event.detail,
        compile_id: event.compile_id,
        crate_name: event.crate_name,
        target_dir: event.target_dir,
        elapsed_ms: event.elapsed_ms,
        exit_code: event.exit_code,
    }) {
        Ok(line) => line,
        Err(_) => return,
    };
    let path = compile_delivery_log_path(paths);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_for(root: &std::path::Path) -> SoldrPaths {
        SoldrPaths::with_root(root.to_path_buf())
    }

    #[test]
    fn record_appends_one_parseable_row_per_event() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths_for(temp.path());

        record(
            &paths,
            &Undelivered {
                kind: UndeliveredKind::ClientDisconnected,
                detail: "eof",
                compile_id: "c00000ab",
                crate_name: Some("protox"),
                target_dir: Some("/work/target"),
                elapsed_ms: 131_700,
                exit_code: None,
            },
        );
        record(
            &paths,
            &Undelivered {
                kind: UndeliveredKind::ReplyWriteFailed,
                detail: "done",
                compile_id: "c00000ac",
                crate_name: None,
                target_dir: None,
                elapsed_ms: 12,
                exit_code: Some(0),
            },
        );

        let text = std::fs::read_to_string(compile_delivery_log_path(&paths)).expect("log written");
        let rows: Vec<serde_json::Value> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("row parses as json"))
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["event"], "client_disconnected");
        assert_eq!(rows[0]["detail"], "eof");
        assert_eq!(rows[0]["crate_name"], "protox");
        assert_eq!(rows[0]["elapsed_ms"], 131_700);
        assert!(rows[0]["exit_code"].is_null());
        assert_eq!(rows[0]["schema_version"], 1);
        // The #1857 signature: a compile that exited 0 and was never
        // handed back. This row is what makes that case countable.
        assert_eq!(rows[1]["event"], "reply_write_failed");
        assert_eq!(rows[1]["exit_code"], 0);
    }

    #[test]
    fn log_path_is_under_the_daemon_state_logs_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths_for(temp.path());
        let path = compile_delivery_log_path(&paths);
        assert!(path.ends_with("compile-delivery.jsonl"), "{path:?}");
        assert!(
            path.parent().is_some_and(|p| p.ends_with("logs")),
            "{path:?}"
        );
    }
}
