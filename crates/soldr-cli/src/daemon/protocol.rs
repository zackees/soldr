//! Wire protocol for soldr-daemon — prost-encoded (Protocol Buffers),
//! length-prefixed frames. Each frame on the wire is:
//!
//! ```text
//! [u32 LE body_len][u32 LE protocol_version][prost body]
//! ```
//!
//! `body_len` does NOT include the 4-byte version field. The body is a
//! prost-encoded `WireRequest` (client → server) or `WireResponse`
//! (server → client) as defined in [`crate::daemon::wire`].
//!
//! ## Serialization (post-#603)
//!
//! Bincode is gone. Every binary surface — IPC body + redb persistent
//! rows — is prost-only. Pre-#580 redb rows are evicted on first
//! daemon startup by the migration in `crate::daemon::db::ensure_initialized`
//! and `crate::cache_lib::cook_index::ensure_initialized`; there is no
//! decode-time fallback anymore.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Protocol version bump rationale:
///
/// * v1–v2: pre-PID-file lifecycle.
/// * v3: target-touch + build-session correlation + linked-zccache.
/// * v4: cook-index IPC (CookLookup, CookRecord, CookTouch) +
///   `cook_stats` on `StatusInfo`. Bincode-bodied wire frames.
/// * v5 (#580): wire body switches from bincode to prost (Protocol
///   Buffers). The header layout is unchanged so the IPC version
///   check still triggers on cross-version traffic.
/// * v5 retained in #603: the wire format did not change — #603 only
///   drops the bincode fallback at the redb layer. Old (v4) clients
///   trying to talk to a v5 daemon (and vice versa) still get a
///   clean "protocol version mismatch" error.
/// * v6 (#977 Phase 5 / #980 L1): adds `Request::Compile` and
///   `Response::CompileResponse` so the rustc-wrapper dispatches every
///   compile to the daemon's embedded `ZccacheService`. The legacy
///   fork-zccache.exe path is gone (#980 L1 second pass) — wrappers
///   that can't reach a v6 daemon hard-error instead of falling back.
/// * v7 (#983 Phase 5b): the daemon stops emitting the single-frame
///   `Response::Compile` for `Request::Compile`. Instead it streams a
///   sequence of `Response::CompileStdoutChunk` / `CompileStderrChunk`
///   frames followed by exactly one terminal `Response::CompileDone`
///   frame. Wrapper-side memory drops from "full rustc stdout+stderr
///   buffered before display" to "at most one chunk in flight". The
///   v6 `CompileResponse` slot is kept on the wire for one release
///   cycle so cross-version traffic still errors as a clean version
///   mismatch instead of a silent oneof mis-decode.
/// * v8 (#1286 F1): adds `Request::FlushCaches` so `soldr save` /
///   `soldr cache flush` can checkpoint the embedded zccache state
///   (artifact index, depgraph snapshot, metadata cache) to disk
///   without shutting the daemon down. Before v8 that state was
///   memory-only until a graceful daemon exit, so archives taken from
///   a live daemon restored with zero rustc hits.
pub const PROTOCOL_VERSION: u32 = 8;

/// Wire-chunk granularity for the streaming Compile reply (#983 Phase
/// 5b). 64 KiB is the same buffer size cargo's own pipe readers use
/// and matches the typical SO_SNDBUF on a Unix socket / named-pipe.
/// Each frame's prost overhead is a handful of bytes so chunk-size is
/// effectively the IPC frame size on the wire; rustc emits stdout /
/// stderr in much smaller increments, but the daemon coalesces those
/// into `CHUNK_BYTES`-sized frames before sending.
pub const CHUNK_BYTES: usize = 64 * 1024;

/// Maximum prost body size. 4 MiB is comfortably above the largest
/// realistic Compile response (rustc stdout/stderr — typically tens of
/// KiB; pathological warning storms a few hundred KiB; tests that
/// emit MB-scale output are vanishingly rare). Bumped from 64 KiB in
/// #977 Phase 5 to accommodate the `Compile` verb's stdout+stderr
/// payload; non-Compile requests stay tiny (hundreds of bytes).
///
/// Streaming the compile output is a Phase 5+ follow-up; until then
/// any compile that exceeds this cap returns an `InvalidData` error
/// to the wrapper which then transparently falls back to the legacy
/// fork-zccache path.
pub const MAX_BODY_BYTES: u32 = 4 * 1024 * 1024;

/// Errors surfaced when decoding wire bytes into the public Rust
/// types. Wraps prost decode failures plus the shape-validation cases
/// proto3 cannot express (fixed-length byte arrays, non-empty oneofs).
#[derive(Debug, Error)]
pub enum WireDecodeError {
    #[error("prost decode error: {0}")]
    Prost(#[from] prost::DecodeError),
    #[error("invalid SHA-256 length on the wire: expected 32 bytes, got {0}")]
    InvalidShaLength(usize),
    #[error("empty {0} oneof — payload missing a discriminant")]
    EmptyOneof(&'static str),
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("unknown event kind discriminant: {0}")]
    UnknownEventKind(u32),
}

#[derive(Debug, Clone)]
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
    /// Request-response: probe the cook-artifact index for the given
    /// `(recipe_hash, target_triple, profile, channel, rustc_version)`
    /// tuple. On exact hit returns [`Response::CookHit`] with
    /// `exact_recipe_match = true`. On exact miss, the daemon may
    /// return a same-origin fallback [`Response::CookHit`] ranked by
    /// `branch_lineage`; otherwise it returns [`Response::CookMiss`].
    CookLookup {
        recipe_hash: [u8; 32],
        target_triple: String,
        profile: String,
        channel: String,
        rustc_version: String,
        origin_url_normalized: Option<String>,
        branch_lineage: Vec<String>,
    },
    /// Request-response: register a cook artifact written by PR 2's
    /// `soldr cook` worker at `~/.soldr/cache/cook/<sha256>.tar.zst`.
    /// Replies with [`Response::Ack`] on success.
    CookRecord {
        recipe_hash: [u8; 32],
        target_triple: String,
        profile: String,
        channel: String,
        rustc_version: String,
        sha256: [u8; 32],
        size_bytes: u64,
        origin_url_normalized: Option<String>,
        branch_name: Option<String>,
        cook_cmd_summary: String,
    },
    /// Fire-and-forget: bump the `last_used_unix_ms` field for the
    /// row whose `sha256` matches.
    CookTouch { sha256: [u8; 32] },
    /// Request-response: dispatch a single rustc invocation to the
    /// daemon's embedded zccache service (issue #977 Phase 5 / #980 L1).
    /// The daemon returns [`Response::CompileResponse`] with the
    /// captured stdout/stderr + exit code, or [`Response::Error`] on
    /// embedded-service failure. There is no longer a wrapper-side
    /// fallback to forking `zccache.exe` — embedded is mandatory.
    Compile(CompileRequest),
    /// Request-response: checkpoint the embedded zccache service's
    /// in-memory state (artifact index, depgraph snapshot, metadata
    /// cache, pending writes) to disk WITHOUT shutting down. Issued by
    /// `soldr save` and `soldr cache flush` before archiving so the
    /// on-disk cache tree is complete (#1286 F1). Replies with
    /// [`Response::Ack`].
    FlushCaches,
}

/// Body of [`Request::Compile`]. Carries the full `rustc` argv plus the
/// surrounding environment so the daemon's embedded service can rebuild
/// the cache key + dispatch the inner compile.
///
/// * `args[0]` is the rustc binary path (e.g. `.../rustc`); the daemon
///   does not re-resolve it, the wrapper has already done the
///   `rustup which rustc` work.
/// * `args[1..]` is the rustc argument list as cargo passed it to the
///   wrapper.
/// * `stdin` is typically empty — rustc reads source from disk. The
///   wrapper's stdin-spill logic ([`crate::wrapper::spill_stdin_to_content_addressed_file`])
///   converts cargo's `-` source-on-stdin probes into a file argument
///   *before* the Compile request is built, so the daemon never has to
///   shuffle a multi-MB stdin payload across the IPC boundary.
#[derive(Debug, Clone)]
pub struct CompileRequest {
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub stdin: Vec<u8>,
}

/// Body of [`Response::CompileResponse`]. Carries the captured rustc
/// output verbatim so the wrapper can replay it onto its own stdout /
/// stderr before exiting with `exit_code`. `cache_outcome` mirrors
/// `zccache::embedded::CacheOutcome` (1=Hit, 2=Miss, 3=Error); the
/// wrapper does not act on it today but it is forwarded so future
/// audit/profiling work can correlate.
#[derive(Debug, Clone)]
pub struct CompileResponseBody {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub cached: bool,
    /// 1=Hit, 2=Miss, 3=Error. Matches the zccache `CacheOutcome`
    /// integer encoding so we don't introduce a soldr-side enum.
    pub cache_outcome: i32,
}

#[derive(Debug, Clone)]
pub enum Response {
    Status(StatusInfo),
    ShuttingDown,
    Builds(Vec<BuildRecord>),
    Error(String),
    /// Reply to [`Request::CookLookup`] on hit. Carries the on-disk
    /// path to the `<sha256>.tar.zst` artifact, the recorded sha256
    /// (PR 3 verifies it before extraction), the byte size for
    /// hydration reporting, the recorded origin URL hint, and whether
    /// the hit was exact or a same-origin fallback seed.
    CookHit {
        sha256: [u8; 32],
        path: String,
        size_bytes: u64,
        origin_url_normalized: Option<String>,
        matched_recipe_hash: Option<[u8; 32]>,
        exact_recipe_match: bool,
        branch_name: Option<String>,
    },
    /// Reply to [`Request::CookLookup`] when neither an exact nor
    /// fallback artifact is available. `previous_origin_recipe_hashes`
    /// is a diagnostic: at most 8 prior recipe hashes recorded under
    /// the same `(origin, triple, profile, channel, rustc)` matrix,
    /// newest-first.
    CookMiss {
        previous_origin_recipe_hashes: Vec<[u8; 32]>,
    },
    /// Generic ack for fire-and-forget-style request/response calls
    /// that don't carry a payload (used by [`Request::CookRecord`]).
    Ack,
    /// Reply to [`Request::Compile`] (issue #977 Phase 5 / #980 L1)
    /// when the daemon's embedded backend handled the rustc dispatch.
    ///
    /// **Deprecated in v7** (#983 Phase 5b): the daemon no longer
    /// emits this variant; it streams [`Response::CompileStdoutChunk`]
    /// / [`Response::CompileStderrChunk`] / [`Response::CompileDone`]
    /// instead. The variant is retained for one release cycle so the
    /// decode path still admits the legacy shape — useful for tooling
    /// that replays captured frames.
    Compile(CompileResponseBody),
    /// One stdout slice in the streaming Compile reply (#983 Phase 5b).
    /// Each chunk is at most [`CHUNK_BYTES`] long; the wrapper relays
    /// it onto its own stdout immediately and discards the buffer.
    CompileStdoutChunk(Vec<u8>),
    /// One stderr slice in the streaming Compile reply (#983 Phase 5b).
    /// Mirrors [`Response::CompileStdoutChunk`] for stderr.
    CompileStderrChunk(Vec<u8>),
    /// Terminal frame in the streaming Compile reply (#983 Phase 5b).
    /// The wrapper reads chunk frames until it sees this variant,
    /// then exits with `exit_code`. `cache_outcome` mirrors the
    /// integer encoding used by [`CompileResponseBody`].
    CompileDone {
        exit_code: i32,
        cached: bool,
        cache_outcome: i32,
        compile_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub version: u32,
    pub pid: u32,
    pub uptime_secs: u64,
    pub request_count: u64,
    /// Set by `LinkZccache`; cleared on daemon shutdown.
    pub linked_zccache: Option<ZccacheDaemonLink>,
    /// Cook-index aggregate stats (issue #576).
    pub cook_stats: Option<CookStats>,
    /// Compile-dispatch backend label. Always [`COMPILE_BACKEND_EMBEDDED`]
    /// since #980 L1's second pass — the legacy "wrapped" fork-zccache.exe
    /// backend is gone. Field retained on the wire so v6 status snapshots
    /// stay stable for telemetry consumers.
    #[serde(default)]
    pub compile_backend: String,
}

/// Canonical `compile_backend` value emitted by the daemon. The
/// "wrapped" label and its associated dispatch path were deleted in
/// #980 L1 second pass; the daemon now always advertises this value.
pub const COMPILE_BACKEND_EMBEDDED: &str = "embedded";

impl StatusInfo {
    /// Resolve `cook_stats` to a concrete value, treating `None` as
    /// the zero state.
    pub fn cook_stats_or_zero(&self) -> CookStats {
        self.cook_stats.clone().unwrap_or_default()
    }
}

/// Aggregate counts for the cook-artifact index. Surfaced through
/// [`Request::Status`] and rendered by `soldr daemon status` /
/// `soldr doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookStats {
    /// Number of rows currently in `cook_index_v2`.
    pub entries: u64,
    /// Sum of `size_bytes` across all rows.
    pub total_bytes: u64,
    /// Number of [`Request::CookLookup`] hits served by the running
    /// daemon since its last startup. Resets across daemon restarts.
    pub hits_this_session: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZccacheDaemonLink {
    pub binary_path: String,
    pub cache_dir: String,
    pub session_id: Option<String>,
    pub source: String,
    pub private_daemon: bool,
    pub daemon_name: Option<String>,
    pub owner_pid: Option<u32>,
    pub private_env_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(protocol_version_is_v7_after_streaming_compile, {
        // Bumped from 6 → 7 in #983 Phase 5b when the daemon switched
        // from a single-frame Response::Compile to streaming
        // CompileStdoutChunk / CompileStderrChunk / CompileDone.
        assert_eq!(PROTOCOL_VERSION, 7);
    });

    crate::timed_test!(chunk_bytes_is_64_kib, {
        // #983 Phase 5b — declared in the protocol so the daemon and
        // wrapper agree on the frame-size budget without re-importing
        // an opaque constant from each other's modules.
        assert_eq!(CHUNK_BYTES, 64 * 1024);
    });

    crate::timed_test!(cook_stats_or_zero_defaults_to_zero, {
        let info = StatusInfo {
            version: PROTOCOL_VERSION,
            pid: 1,
            uptime_secs: 0,
            request_count: 0,
            linked_zccache: None,
            cook_stats: None,
            compile_backend: COMPILE_BACKEND_EMBEDDED.to_string(),
        };
        assert_eq!(info.cook_stats_or_zero(), CookStats::default());
    });
}
