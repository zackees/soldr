//! Wire protocol for soldr-daemon — bincode-encoded, length-prefixed
//! frames. Each frame on the wire is:
//!
//! ```text
//! [u32 LE body_len][u32 LE protocol_version][bincode body]
//! ```
//!
//! `body_len` does NOT include the 4-byte version field. The body is a
//! bincoded `Request` (client → server) or `Response` (server → client).

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 2;

/// Maximum bincode body size. 64 KiB is comfortably above the largest
/// realistic record (path strings, a few timestamps). Frames larger than
/// this are rejected on both ends to harden against a misbehaving peer.
pub const MAX_BODY_BYTES: u32 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Fire-and-forget: stamp a workspace `target/` path with `unix_seconds`
    /// in the soldr target registry. The wrapper hot path uses this.
    RecordTargetTouch { path: String, unix_seconds: i64 },
    /// Request-response: return a small structured snapshot of daemon
    /// state. Used by `soldr daemon status`.
    Status,
    /// Request-response: ask the daemon to drain and exit. Used by
    /// `soldr daemon stop` and (Phase 3) linked-zccache shutdown.
    Shutdown,
    /// Fire-and-forget: open a build session. Issued by the cargo
    /// front door immediately before spawning cargo.
    BuildSessionStart {
        session_id: u64,
        repo_root: String,
        started_at_ms: i64,
    },
    /// Fire-and-forget: finalize a build session. Issued by the cargo
    /// front door after cargo exits.
    BuildSessionEnd {
        session_id: u64,
        exit_code: i32,
        ended_at_ms: i64,
    },
    /// Fire-and-forget: record one rustc invocation inside a build
    /// session. `duration_us` is `None` on Unix where the wrapper
    /// `exec()`s into zccache and never returns (only the start event
    /// is observable from soldr's side); on Windows the wrapper waits
    /// for the spawned process and fills the duration.
    RecordCompile {
        session_id: u64,
        crate_name: String,
        target_dir: String,
        started_at_ms: i64,
        duration_us: Option<u64>,
    },
    /// Request-response: return the most recent build records, newest
    /// first, up to `limit`. Optional `since_ms` filters to records
    /// whose `started_at_ms >= since_ms`.
    ListBuilds { limit: u32, since_ms: Option<i64> },
    /// Request-response: return finished build records whose
    /// `total_wall_ms >= threshold_ms`, sorted desc by `total_wall_ms`,
    /// capped at `limit`.
    ListSlowBuilds { threshold_ms: u64, limit: u32 },
    /// Fire-and-forget: tell the daemon which zccache runtime/cache/session is
    /// linked to this soldr-daemon's session. On daemon shutdown
    /// (explicit RPC, signal, or idle timeout), the daemon issues
    /// `zccache stop` with the recorded cache dir before exiting.
    LinkZccache { link: ZccacheDaemonLink },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Status(StatusInfo),
    ShuttingDown,
    Builds(Vec<BuildRecord>),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusInfo {
    pub version: u32,
    pub pid: u32,
    pub uptime_secs: u64,
    pub request_count: u64,
    /// Set by `LinkZccache`; cleared on daemon shutdown.
    pub linked_zccache: Option<ZccacheDaemonLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZccacheDaemonLink {
    pub binary_path: String,
    pub cache_dir: String,
    pub session_id: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildRecord {
    pub session_id: u64,
    pub repo_root: String,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub exit_code: Option<i32>,
    pub total_wall_ms: Option<u64>,
    pub crate_count: u32,
    pub slowest_crate_us: Option<u64>,
    pub slowest_crate_name: Option<String>,
}
