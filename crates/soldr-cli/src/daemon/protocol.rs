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

pub const PROTOCOL_VERSION: u32 = 1;

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
    /// `soldr daemon stop` and (Phase 3) by linked-zccache shutdown.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Status(StatusInfo),
    ShuttingDown,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusInfo {
    pub version: u32,
    pub pid: u32,
    pub uptime_secs: u64,
    pub request_count: u64,
    /// Phase 3 will populate this with the linked zccache daemon's PID.
    pub linked_zccache_pid: Option<u32>,
}
