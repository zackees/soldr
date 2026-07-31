//! Synchronous, blocking client used by the wrapper hot path and by the
//! `soldr daemon status|stop` CLI surface. All daemon calls are best
//! effort: every error variant is mapped to a `ClientError` so the
//! caller can decide whether to fall back to direct redb writes.

use crate::cache_lib::target_registry::{current_unix_seconds, TargetRegistry};
use crate::cache_lib::{daemon_sock_path, data_db_path};
use crate::core::SoldrPaths;
#[cfg(windows)]
use crate::daemon::ipc::{
    read_frame_async, read_frame_async_for_version, write_frame_async,
    write_frame_async_for_version,
};
#[cfg(unix)]
use crate::daemon::ipc::{
    read_frame_sync, read_frame_sync_for_version, write_frame_sync, write_frame_sync_for_version,
};
use crate::daemon::protocol::{
    BuildRecord, CacheFlushInfo, CompileRequest, CompileStatsInfo, Request, Response, ShutdownAck,
    StatusInfo,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 50 ms write timeout matches the plan: short enough that the wrapper
/// never blocks the hot path on a hung daemon.
const HOT_PATH_TIMEOUT: Duration = Duration::from_millis(50);

/// Slightly more generous timeout for request/response calls that need
/// to read a body back (status, shutdown). Still small enough that the
/// CLI returns quickly even if the daemon is unresponsive.
const REPLY_TIMEOUT: Duration = Duration::from_millis(2_000);
/// The embedded flush has seven individually bounded phases (pending writes,
/// index writer, and up to five persistence saves), so its IPC budget must be
/// longer than the generic status/shutdown request timeout.
const CACHE_FLUSH_REPLY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Last protocol spoken by pre-generation shutdown acknowledgements. This
/// fallback exists only to retire an older local daemon safely during rollout.
const LEGACY_SHUTDOWN_PROTOCOL_VERSION: u32 = 17;

/// Default compile-dispatch timeout — rustc may take minutes for a release
/// build of a large crate, so the default stays generous (30 minutes): a
/// stuck rustc cannot wedge the wrapper forever, but any legitimate compile
/// inside that bound runs to completion. Issue #977 Phase 5 / #980 L1.
pub const DEFAULT_REPLY_TIMEOUT_SECS: u64 = 30 * 60;

/// Env override for [`compile_reply_timeout`] (issue #1364). Lets an
/// operator fail fast on a wedged cache without waiting out the 30-minute
/// backstop, e.g. `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=30`. `0`, empty, or an
/// unparseable value falls back to [`DEFAULT_REPLY_TIMEOUT_SECS`].
pub const REPLY_TIMEOUT_ENV: &str = "SOLDR_COMPILE_REPLY_TIMEOUT_SECS";

#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;
#[cfg(windows)]
const PIPE_BUSY_RETRY_LIMIT: u32 = 8;

/// How many times a client re-dials after the daemon replies
/// `Response::Backpressure`. Shared by both transports since soldr#1853 —
/// compile admission is no longer Windows-only, so the AF_UNIX path needs
/// the same bounded back-off.
const BACKPRESSURE_RETRY_LIMIT: u32 = 8;

#[cfg(windows)]
struct WindowsPipeOpen {
    stream: tokio::net::windows::named_pipe::NamedPipeClient,
    busy_retries: u32,
}

#[cfg(windows)]
fn busy_pipe_retry_delay(attempt: u32) -> Duration {
    let base_ms = (2_u64.saturating_mul(1_u64 << attempt.min(5))).min(64);
    // A tiny deterministic jitter avoids synchronized cargo workers while
    // keeping this retry policy testable and dependency-free.
    let jitter_ms = (u64::from(attempt) * 17 + u64::from(std::process::id())) % 4;
    Duration::from_millis(base_ms + jitter_ms)
}

#[cfg(windows)]
async fn open_windows_pipe_with_retry(path: &Path) -> std::io::Result<WindowsPipeOpen> {
    use tokio::net::windows::named_pipe::ClientOptions;
    for attempt in 0..PIPE_BUSY_RETRY_LIMIT {
        match ClientOptions::new().open(path) {
            Ok(stream) => {
                return Ok(WindowsPipeOpen {
                    stream,
                    busy_retries: attempt,
                })
            }
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                    && attempt + 1 < PIPE_BUSY_RETRY_LIMIT =>
            {
                // A busy pipe is listener-pool backpressure, not evidence that the
                // daemon died. Keep retrying inside this client call so callers do
                // not start recovery or bypass the cache.
                tokio::time::sleep(busy_pipe_retry_delay(attempt)).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("retry loop either returned a client or its last IO error")
}

/// Compile-dispatch timeout, resolved once from the environment.
pub fn compile_reply_timeout() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| parse_reply_timeout(std::env::var(REPLY_TIMEOUT_ENV).ok().as_deref()))
}

/// Pure policy for [`compile_reply_timeout`] so the parse/fallback matrix
/// is unit-testable without mutating the process env.
fn parse_reply_timeout(value: Option<&str>) -> Duration {
    let secs = value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_REPLY_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

#[derive(Debug)]
pub enum ClientError {
    /// No daemon endpoint exists at the expected path (most common case
    /// on a fresh checkout — caller should fall back to direct redb).
    NotRunning,
    /// Endpoint exists but the connect / read / write failed.
    Io(std::io::Error),
    /// Daemon answered something we didn't ask for (or an Error variant).
    Protocol(String),
    /// The daemon answered, but speaks a protocol this binary cannot parse
    /// (#1853).
    ///
    /// Distinct from [`Self::Protocol`] because retrying cannot help: a
    /// version-skewed daemon can never serve this client, no matter how many
    /// attempts remain. The caller must displace the daemon or fall back to a
    /// direct compile rather than burning its retry budget.
    VersionMismatch(String),
    /// The daemon answered that it is retiring and will not serve the
    /// request (soldr#1838 Phase 2).
    ///
    /// Distinct from [`Self::Protocol`] for the same reason as
    /// [`Self::VersionMismatch`]: the daemon is not misbehaving, it simply
    /// cannot help, so degrading to a direct compile masks nothing. Folding
    /// this into `Protocol` is what failed builds during a normal graceful
    /// drain (#1837).
    Retiring,
    /// A compile reply deadline expired (soldr#1838 Phase 2, bullet 4).
    ///
    /// `saw_output` is the whole point: it separates *slow* from *wedged*,
    /// which need opposite advice. A compile that streamed diagnostics and
    /// then ran out of clock is a long build hitting the backstop -- raising
    /// `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` is the fix, and bypassing the cache
    /// would only make it slower. A compile that produced nothing at all is
    /// the wedge described in #1364, where bypassing is the fix and raising
    /// the timeout just prolongs the hang.
    ///
    /// Carried as its own variant rather than an enriched `Io` because the
    /// distinction has to survive to the message-formatting site, and
    /// `io::Error` can only carry it as prose.
    CompileStalled { saw_output: bool, elapsed: Duration },
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        #[cfg(windows)]
        if e.raw_os_error() == Some(ERROR_PIPE_BUSY) {
            // The retry cap only bounds a single client call; it does not
            // change daemon liveness. Keep this out of the caller's generic
            // I/O-recovery branch, which is allowed to spawn a missing daemon.
            return ClientError::Protocol(
                "Windows named pipe remained busy after retry budget".into(),
            );
        }
        // #1853: both the version-mismatch branch and the pre-handshake
        // reject record surface as InvalidData from the frame reader. Classify
        // them here, in the shared conversion, so Unix and Windows transports
        // both get it from one place.
        if e.kind() == std::io::ErrorKind::InvalidData {
            let message = e.to_string();
            if message.contains("protocol version mismatch")
                || message.contains("daemon rejected connection")
            {
                return ClientError::VersionMismatch(message);
            }
        }
        match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                ClientError::NotRunning
            }
            _ => ClientError::Io(e),
        }
    }
}

/// Submit `req` to the daemon and drop the connection without reading
/// any reply. Used for `RecordTargetTouch`. The wrapper hot path calls
/// this and ignores the result on the failure side.
pub fn submit_fire_and_forget(sock_path: &Path, req: &Request) -> Result<(), ClientError> {
    #[cfg(windows)]
    {
        submit_fire_and_forget_windows(sock_path, req)
    }
    #[cfg(unix)]
    {
        let mut stream = connect(sock_path, HOT_PATH_TIMEOUT)?;
        write_frame_sync(&mut stream, req)?;
        Ok(())
    }
}

/// Submit `req`, wait for one `Response`, return it.
pub fn submit_request(sock_path: &Path, req: &Request) -> Result<Response, ClientError> {
    #[cfg(windows)]
    {
        submit_request_windows(sock_path, req)
    }
    #[cfg(unix)]
    {
        let mut stream = connect(sock_path, REPLY_TIMEOUT)?;
        write_frame_sync(&mut stream, req)?;
        let resp: Response = read_frame_sync(&mut stream)?;
        Ok(resp)
    }
}

fn submit_request_for_version(
    sock_path: &Path,
    req: &Request,
    protocol_version: u32,
) -> Result<Response, ClientError> {
    #[cfg(windows)]
    {
        submit_request_windows_with_timeout_and_version(
            sock_path,
            req,
            REPLY_TIMEOUT,
            protocol_version,
        )
    }
    #[cfg(unix)]
    {
        let mut stream = connect(sock_path, REPLY_TIMEOUT)?;
        write_frame_sync_for_version(&mut stream, req, protocol_version)?;
        let resp: Response = read_frame_sync_for_version(&mut stream, protocol_version)?;
        Ok(resp)
    }
}

pub fn status(sock_path: &Path) -> Result<StatusInfo, ClientError> {
    match submit_request(sock_path, &Request::Status)? {
        Response::Status(info) => Ok(info),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// soldr#1368: read the embedded zccache service's cumulative compile
/// counters. `soldr session start` captures a baseline and `soldr
/// session end` diffs against it to report per-session hit/miss stats —
/// replacing the removed managed `zccache session-end` subprocess.
pub fn compile_stats(sock_path: &Path) -> Result<CompileStatsInfo, ClientError> {
    match submit_request(sock_path, &Request::CompileStats)? {
        Response::CompileStats(info) => Ok(info),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn shutdown(sock_path: &Path) -> Result<ShutdownAck, ClientError> {
    let current_error = match submit_request(sock_path, &Request::Shutdown) {
        Ok(Response::ShuttingDown(ack)) => return Ok(ack),
        Ok(Response::Error(msg)) => return Err(ClientError::Protocol(msg)),
        Ok(other) => {
            return Err(ClientError::Protocol(format!(
                "unexpected response: {other:?}"
            )))
        }
        Err(ClientError::NotRunning) => return Err(ClientError::NotRunning),
        Err(error) => error,
    };

    // v18 rollout bridge: v17's empty shutdown Ack cannot identify its
    // responder. Read v17 status immediately before requesting shutdown, then
    // use that PID only for waiting. Callers never signal this legacy identity.
    let legacy_status = match submit_request_for_version(
        sock_path,
        &Request::Status,
        LEGACY_SHUTDOWN_PROTOCOL_VERSION,
    ) {
        Ok(Response::Status(status)) => Some(status),
        _ => None,
    };
    match submit_request_for_version(
        sock_path,
        &Request::Shutdown,
        LEGACY_SHUTDOWN_PROTOCOL_VERSION,
    ) {
        Ok(Response::ShuttingDown(ack)) if ack.pid != 0 => Ok(ack),
        Ok(Response::ShuttingDown(_)) => legacy_status
            .map(|status| ShutdownAck {
                pid: status.pid,
                generation: status.generation,
            })
            .ok_or_else(|| {
                ClientError::Protocol(
                    "legacy daemon acknowledged shutdown without a responder identity".into(),
                )
            }),
        _ => Err(current_error),
    }
}

/// Issue #1286 (F1): ask the daemon to checkpoint the embedded zccache
/// state (artifact index, depgraph snapshot, metadata cache) to disk
/// without shutting down. Used by `soldr save` / `soldr cache flush`
/// before archiving the cache tree.
pub fn flush_caches(sock_path: &Path) -> Result<CacheFlushInfo, ClientError> {
    // soldr#1838: the embedded flush has seven individually bounded phases
    // and can legitimately take minutes on a large cache. Report progress
    // rather than going silent for the whole 5-minute budget. There is no
    // env override for this one, hence `None`.
    let _heartbeat = super::wait_heartbeat::WaitHeartbeat::start(
        "daemon cache flush",
        CACHE_FLUSH_REPLY_TIMEOUT,
        None,
    );
    match submit_request_with_timeout(sock_path, &Request::FlushCaches, CACHE_FLUSH_REPLY_TIMEOUT)?
    {
        Response::CacheFlushed(info) => Ok(info),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn list_builds(
    sock_path: &Path,
    limit: u32,
    since_ms: Option<i64>,
) -> Result<Vec<BuildRecord>, ClientError> {
    match submit_request(sock_path, &Request::ListBuilds { limit, since_ms })? {
        Response::Builds(rows) => Ok(rows),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Fetch the build-log inputs the daemon owns for `session_id`
/// (soldr#1814 slice 2a).
///
/// Returns the session's event rows plus its build record, if any. Callers
/// fall back to opening the state DB directly only when this errors — see
/// `build_log::write_build_log`.
#[allow(clippy::type_complexity)]
pub fn build_log_inputs(
    sock_path: &Path,
    session_id: u64,
) -> Result<(Vec<crate::daemon::db::Event>, Option<Box<BuildRecord>>), ClientError> {
    match submit_request(sock_path, &Request::BuildLogInputs { session_id })? {
        Response::BuildLogInputs { events, record } => Ok((events, record)),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Ask the daemon whether to emit the cargo-debug-default warning for
/// `repo_root` (soldr#1814 slice 2c).
///
/// The daemon owns the throttle state, so asking it also records the repo.
/// Callers fall back to the local read-modify-write only when the daemon is
/// unreachable.
pub fn should_warn_cargo_debug_default(
    sock_path: &Path,
    repo_root: &Path,
) -> Result<bool, ClientError> {
    let req = Request::ShouldWarnCargoDebugDefault {
        repo_root: repo_root.display().to_string(),
    };
    match submit_request(sock_path, &req)? {
        Response::CargoDebugWarning { emit } => Ok(emit),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Ask the daemon to merge this build's log-history results into its record
/// (soldr#1814 slice 2d).
///
/// The daemon performs the whole read-modify-write under its own ownership of
/// the table, so callers must not also open the DB on success.
pub fn attach_build_log_history(
    sock_path: &Path,
    update: crate::daemon::protocol::BuildLogHistoryUpdate,
) -> Result<(), ClientError> {
    match submit_request(sock_path, &Request::AttachBuildLogHistory(Box::new(update)))? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn list_slow_builds(
    sock_path: &Path,
    threshold_ms: u64,
    limit: u32,
) -> Result<Vec<BuildRecord>, ClientError> {
    match submit_request(
        sock_path,
        &Request::ListSlowBuilds {
            threshold_ms,
            limit,
        },
    )? {
        Response::Builds(rows) => Ok(rows),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Acknowledged build-session lifecycle helper. The daemon only returns Ok
/// after the start record and SessionStart event are persisted.
pub fn build_session_start(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    started_at_ms: i64,
) -> Result<DaemonCompileLimit, ClientError> {
    let sock = default_sock_path(paths);
    match submit_request(
        &sock,
        &Request::BuildSessionStart {
            session_id,
            repo_root: repo_root.display().to_string(),
            started_at_ms,
        },
    )? {
        Response::BuildSessionStarted {
            compile_jobs,
            compile_jobs_source,
        } => Ok(DaemonCompileLimit {
            jobs: compile_jobs as usize,
            source: compile_jobs_source,
        }),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected build_session_start response: {other:?}"
        ))),
    }
}

/// The compile limit a running daemon reported (soldr#2023).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonCompileLimit {
    pub jobs: usize,
    /// Human-readable precedence tier, from `JobsSource::describe`.
    pub source: String,
}

/// Acknowledged finalization (soldr#1536): blocks until the daemon has
/// rolled the session up from its in-memory aggregate, persisted the
/// finalized BuildRecord, and flushed every staged session event to
/// redb. `Ok(())` therefore means the wrapper can trust the persisted
/// aggregate instead of re-scanning the event table; any error routes
/// the caller to the direct-redb fallback exactly like before.
pub fn build_session_end(
    paths: &SoldrPaths,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
) -> Result<(), ClientError> {
    let sock = default_sock_path(paths);
    match submit_request(
        &sock,
        &Request::BuildSessionEnd {
            session_id,
            exit_code,
            ended_at_ms,
        },
    )? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected build_session_end response: {other:?}"
        ))),
    }
}

/// PR 1 cook-index client surface (#576). PR 2 (`soldr cook`) and PR 3
/// (cargo-front-door pre-flight) consume these; PR 1 ships dormant so
/// the helpers here are unused outside integration tests.
///
/// The reply is a [`CookLookupOutcome`]:
/// * `Hit { sha256, path, size_bytes, origin_url_normalized }` — PR 3
///   verifies `sha256` against the bytes at `path` before extracting.
/// * `Miss { previous_origin_recipe_hashes }` — used as a drift
///   diagnostic when the pre-flight misses.
///
/// `Err(ClientError::NotRunning)` means the daemon endpoint is not
/// reachable — caller must NOT treat this as a hard error; the hot
/// path falls through to a normal cargo run.
#[allow(clippy::too_many_arguments)]
pub fn cook_lookup(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    origin_url_normalized: Option<String>,
) -> Result<CookLookupOutcome, ClientError> {
    cook_lookup_with_branch_lineage(
        sock_path,
        recipe_hash,
        target_triple,
        profile,
        channel,
        rustc_version,
        origin_url_normalized,
        Vec::new(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn cook_lookup_with_branch_lineage(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    origin_url_normalized: Option<String>,
    branch_lineage: Vec<String>,
) -> Result<CookLookupOutcome, ClientError> {
    let req = Request::CookLookup {
        recipe_hash,
        target_triple,
        profile,
        channel,
        rustc_version,
        origin_url_normalized,
        branch_lineage,
    };
    match submit_request(sock_path, &req)? {
        Response::CookHit {
            sha256,
            path,
            size_bytes,
            origin_url_normalized,
            matched_recipe_hash,
            exact_recipe_match,
            branch_name,
        } => Ok(CookLookupOutcome::Hit {
            sha256,
            path,
            size_bytes,
            origin_url_normalized,
            matched_recipe_hash,
            exact_recipe_match,
            branch_name,
        }),
        Response::CookMiss {
            previous_origin_recipe_hashes,
        } => Ok(CookLookupOutcome::Miss {
            previous_origin_recipe_hashes,
        }),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected cook_lookup response: {other:?}"
        ))),
    }
}

/// Strongly-typed reply for [`cook_lookup`] / [`cook_lookup_full`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookLookupOutcome {
    Hit {
        sha256: [u8; 32],
        path: String,
        size_bytes: u64,
        origin_url_normalized: Option<String>,
        matched_recipe_hash: Option<[u8; 32]>,
        exact_recipe_match: bool,
        branch_name: Option<String>,
    },
    Miss {
        previous_origin_recipe_hashes: Vec<[u8; 32]>,
    },
}

/// Register a cook artifact with the daemon. PR 2's `soldr cook`
/// worker calls this after writing `<sha256>.tar.zst` to
/// `~/.soldr/cache/cook/`. Blocks for the daemon's `Ack` reply
/// because PR 2 wants to know whether the indexing succeeded before
/// emitting its `soldr cook: indexed` line.
#[allow(clippy::too_many_arguments)]
pub fn cook_record(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    sha256: [u8; 32],
    size_bytes: u64,
    origin_url_normalized: Option<String>,
    cook_cmd_summary: String,
) -> Result<(), ClientError> {
    cook_record_with_branch(
        sock_path,
        recipe_hash,
        target_triple,
        profile,
        channel,
        rustc_version,
        sha256,
        size_bytes,
        origin_url_normalized,
        None,
        cook_cmd_summary,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn cook_record_with_branch(
    sock_path: &Path,
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
) -> Result<(), ClientError> {
    let req = Request::CookRecord {
        recipe_hash,
        target_triple,
        profile,
        channel,
        rustc_version,
        sha256,
        size_bytes,
        origin_url_normalized,
        branch_name,
        cook_cmd_summary,
    };
    match submit_request(sock_path, &req)? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected cook_record response: {other:?}"
        ))),
    }
}

/// Fire-and-forget bump of `last_used_unix_ms` for the entry whose
/// sha256 matches. Best-effort by design: a touch failure must never
/// affect callers (eviction will simply observe stale `last_used`).
pub fn cook_touch(sock_path: &Path, sha256: [u8; 32]) -> Result<(), ClientError> {
    submit_fire_and_forget(sock_path, &Request::CookTouch { sha256 })
}

/// Metadata returned by the streaming compile call after the last
/// chunk frame has been consumed (#983 Phase 5b). Carries everything
/// the wrapper needs to surface to cargo — the captured rustc output
/// has already been written through to the caller-provided sinks.
#[derive(Debug, Clone)]
pub struct CompileDoneInfo {
    pub exit_code: i32,
    #[allow(dead_code)] // forwarded for future telemetry; wrapper does not act on it today
    pub cached: bool,
    #[allow(dead_code)] // 1=Hit, 2=Miss, 3=Error — same shape as CompileResponseBody
    pub cache_outcome: i32,
    /// Daemon-side audit id for the compile. Empty in Phase 5b1 (the
    /// embedded zccache service does not yet surface it). Plumbed
    /// through so Phase 5b2 can fill it without another version bump.
    #[allow(dead_code)]
    pub compile_id: String,
}

/// Dispatch a single rustc compile to the daemon's embedded zccache
/// service and stream the captured rustc stdout/stderr to the caller's
/// sinks as they arrive (#983 Phase 5b — superseded the v6 single-frame
/// reply path).
///
/// The reply timeout is generous (30 minutes) because rustc itself may
/// take many minutes for a release build of a large crate. The function
/// reads frames in a loop, dispatching `CompileStdoutChunk` /
/// `CompileStderrChunk` to the matching writer and returning once it
/// sees the terminal `CompileDone` frame.
///
/// On any timeout / IO error the wrapper hard-errors (the legacy
/// `zccache.exe` fork path was removed in #980 L1's second pass).
/// Whether an IO error is a reply-deadline expiry rather than a real
/// transport fault. `WouldBlock` is included because a socket read timeout
/// surfaces as either kind depending on platform.
fn is_deadline_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

pub fn compile_streaming<O, E>(
    sock_path: &Path,
    req: CompileRequest,
    mut stdout: O,
    mut stderr: E,
) -> Result<CompileDoneInfo, ClientError>
where
    O: Write,
    E: Write,
{
    #[cfg(windows)]
    {
        compile_streaming_windows(sock_path, req, &mut stdout, &mut stderr)
    }
    #[cfg(unix)]
    {
        // Honour `Response::Backpressure` (soldr#1853). Compile admission
        // used to be `#[cfg(windows)]`-only, so this transport accepted every
        // connection unconditionally and shed load by resetting sockets —
        // surfacing as ECONNRESET, a burned 30 s budget, and a red build.
        // Applying admission on Unix needs a client that backs off instead of
        // treating the reply as a protocol violation, which is exactly what
        // `open_compile_pipe_with_backpressure` already does for named pipes.
        //
        // Synchronous by necessity: this path is sync end to end, so the wait
        // is a `thread::sleep` and each retry re-dials. The first frame is
        // carried into the loop below rather than re-read, since reading it
        // here to test for backpressure consumes it.
        let (mut stream, first_frame) = {
            let mut admitted = None;
            let mut last_retry_after_ms = 0u32;
            // soldr#1838: how long we have been trying to get a first frame,
            // reported when that wait is what expires.
            let dial_started = std::time::Instant::now();
            for attempt in 0..BACKPRESSURE_RETRY_LIMIT {
                let mut stream = connect(sock_path, compile_reply_timeout())?;
                write_frame_sync(&mut stream, &Request::Compile(req.clone()))?;
                // soldr#1838: `read_frame_sync` blocks for the whole compile
                // budget (30 min by default). Report progress while it does,
                // rather than going silent until the backstop expires. The
                // guard stops on drop, so a fast compile prints nothing.
                let _heartbeat = super::wait_heartbeat::WaitHeartbeat::start(
                    "daemon compile reply",
                    compile_reply_timeout(),
                    Some(REPLY_TIMEOUT_ENV),
                );
                // soldr#1838: a daemon that accepts the connection and then
                // never sends a first frame IS the wedge case -- nothing
                // arrived at all. The bare `?` here mapped that to a generic
                // `Io` error, so the slow-vs-wedged signal the Windows
                // transport already reports was lost on this transport, and
                // the wrapper could not tell the user which remedy applies.
                // Found by the #1838 Phase 4 fault-injection harness.
                let frame: Response = match read_frame_sync(&mut stream) {
                    Ok(frame) => frame,
                    Err(err) if is_deadline_error(&err) => {
                        return Err(ClientError::CompileStalled {
                            saw_output: false,
                            elapsed: dial_started.elapsed(),
                        });
                    }
                    Err(err) => return Err(ClientError::from(err)),
                };
                match frame {
                    Response::Backpressure { retry_after_ms } => {
                        last_retry_after_ms = retry_after_ms;
                        if attempt + 1 == BACKPRESSURE_RETRY_LIMIT {
                            break;
                        }
                        // Per-process jitter so wrappers released together do
                        // not re-dial in lockstep; mirrors the Windows spread.
                        let jitter_ms =
                            (u64::from(attempt) * 11 + u64::from(std::process::id())) % 4;
                        std::thread::sleep(Duration::from_millis(
                            u64::from(retry_after_ms) + jitter_ms,
                        ));
                    }
                    other => {
                        admitted = Some((stream, other));
                        break;
                    }
                }
            }
            match admitted {
                Some(pair) => pair,
                // Deliberately `Io`, not `Protocol`: a daemon too busy to
                // admit us is *unavailable*, and `Protocol` is classified as
                // "the daemon answered, so do not degrade"
                // (`compile_dispatch::client_error_indicates_daemon_unavailable`).
                // Reporting this as Io lets the caller fall back to a direct
                // uncached rustc instead of failing the build.
                None => {
                    return Err(ClientError::Io(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        format!(
                            "daemon IPC admission stayed backpressured across                              {BACKPRESSURE_RETRY_LIMIT} attempts                              ({last_retry_after_ms}ms apart)"
                        ),
                    )))
                }
            }
        };
        let mut pending = Some(first_frame);
        // soldr#1838 bullet 4: whether the compile ever spoke separates a slow
        // build from a wedged daemon, and they need opposite advice. Tracked
        // here because this is the only place that sees both the chunks and
        // the deadline.
        let started = std::time::Instant::now();
        let mut saw_output = false;
        // soldr#1838 Phase 1: the streaming phase is where a compile actually
        // stalls, and it used to run silent -- the heartbeat above only covers
        // the wait for the first frame. Publish chunk arrivals so each beat can
        // say whether output is still coming.
        let progress = super::wait_heartbeat::StreamProgress::new();
        let _stream_heartbeat = super::wait_heartbeat::WaitHeartbeat::start_streaming(
            "daemon compile stream",
            compile_reply_timeout(),
            Some(REPLY_TIMEOUT_ENV),
            std::sync::Arc::clone(&progress),
        );
        loop {
            let frame: Response = match pending.take() {
                Some(frame) => frame,
                // `read_frame_sync` yields `io::Result`, which the original
                // `?` converted through `From<io::Error>`. Match the io error
                // directly and keep that conversion for everything that is not
                // a deadline.
                None => match read_frame_sync(&mut stream) {
                    Ok(frame) => frame,
                    Err(err) if is_deadline_error(&err) => {
                        return Err(ClientError::CompileStalled {
                            saw_output,
                            elapsed: started.elapsed(),
                        });
                    }
                    Err(err) => return Err(ClientError::from(err)),
                },
            };
            match frame {
                Response::CompileStdoutChunk(bytes) => {
                    saw_output = true;
                    progress.record_chunk();
                    tracing::debug!(
                        target: "soldr::client::compile_stream",
                        bytes = bytes.len(),
                        "stdout chunk received",
                    );
                    stdout.write_all(&bytes).map_err(ClientError::Io)?;
                }
                Response::CompileStderrChunk(bytes) => {
                    saw_output = true;
                    progress.record_chunk();
                    tracing::debug!(
                        target: "soldr::client::compile_stream",
                        bytes = bytes.len(),
                        "stderr chunk received",
                    );
                    stderr.write_all(&bytes).map_err(ClientError::Io)?;
                }
                Response::CompileDone {
                    exit_code,
                    cached,
                    cache_outcome,
                    compile_id,
                } => {
                    tracing::debug!(
                        target: "soldr::client::compile_stream",
                        exit_code,
                        cached,
                        cache_outcome,
                        "compile done — streaming reply complete",
                    );
                    return Ok(CompileDoneInfo {
                        exit_code,
                        cached,
                        cache_outcome,
                        compile_id,
                    });
                }
                Response::Error(msg) => return Err(ClientError::Protocol(msg)),
                // soldr#1838 Phase 2: keep this above the catch-all -- falling
                // through would turn a well-behaved "I am retiring" into a
                // protocol violation and deny the direct-rustc fallback.
                Response::Retiring => return Err(ClientError::Retiring),
                other => {
                    return Err(ClientError::Protocol(format!(
                        "unexpected compile stream frame: {other:?}"
                    )));
                }
            }
        }
    }
}

/// Same shape as [`submit_request`] but with an explicit reply timeout.
/// Extracted so [`compile`] can use a 30-minute budget without bloating
/// the call surface of the generic helper.
pub fn submit_request_with_timeout(
    sock_path: &Path,
    req: &Request,
    timeout: Duration,
) -> Result<Response, ClientError> {
    #[cfg(windows)]
    {
        submit_request_windows_with_timeout(sock_path, req, timeout)
    }
    #[cfg(unix)]
    {
        let mut stream = connect(sock_path, timeout)?;
        write_frame_sync(&mut stream, req)?;
        let resp: Response = read_frame_sync(&mut stream)?;
        Ok(resp)
    }
}

/// Wrapper-side entry point. Tries the daemon first; on any failure,
/// upserts the row directly to the redb file. **Never** propagates
/// errors — a missing daemon must not break a build.
///
/// Issue #1814: the direct-redb leg is the *second* opener of `state.redb` on
/// a hot path, so it uses the short best-effort budget rather than blocking a
/// compile for up to 5 s behind the daemon's own handle. Both the reason for
/// taking the fallback and a failed fallback are now reported — this used to
/// be two silent `let _ =`s, which is precisely how state-DB contention became
/// an unexplained stall with no reason attached (cf. zccache#1211).
pub fn record_target_touch_or_fallback(paths: &SoldrPaths, target: &Path) {
    let unix_seconds = match current_unix_seconds() {
        Ok(s) => s,
        Err(_) => return,
    };

    let sock = default_sock_path(paths);
    let req = Request::RecordTargetTouch {
        path: target.display().to_string(),
        unix_seconds,
    };
    let daemon_error = match submit_fire_and_forget(&sock, &req) {
        Ok(()) => return,
        Err(error) => error,
    };

    let db_path = data_db_path(paths);
    match TargetRegistry::open_best_effort(&db_path) {
        Ok(registry) => {
            if let Err(error) = registry.upsert_with_time(target, unix_seconds) {
                tracing::warn!(
                    event = "target_touch_fallback_write_failed",
                    target = %target.display(),
                    daemon_error = ?daemon_error,
                    error = %error,
                    "target-registry touch was lost: daemon unreachable and the \
                     direct redb write failed"
                );
            }
        }
        Err(error) => {
            // `open_best_effort` already emitted the durable contention record
            // when this was lock contention; this line attaches the *reason we
            // were on the fallback path at all*.
            tracing::warn!(
                event = "target_touch_fallback_open_failed",
                target = %target.display(),
                daemon_error = ?daemon_error,
                error = %error,
                "target-registry touch was skipped: daemon unreachable and the \
                 state DB could not be opened within the best-effort budget"
            );
        }
    }
}

#[cfg(unix)]
fn connect(sock_path: &Path, timeout: Duration) -> Result<UnixOrPipe, ClientError> {
    let stream = std::os::unix::net::UnixStream::connect(sock_path)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout.max(Duration::from_millis(200))))?;
    Ok(UnixOrPipe(stream))
}

#[cfg(unix)]
pub struct UnixOrPipe(std::os::unix::net::UnixStream);

#[cfg(unix)]
impl std::io::Read for UnixOrPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

#[cfg(unix)]
impl std::io::Write for UnixOrPipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[cfg(windows)]
fn windows_timeout_error(operation: &str, timeout: Duration) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("{operation} timed out after {}ms", timeout.as_millis()),
    )
}

#[cfg(windows)]
fn windows_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
}

#[cfg(windows)]
async fn open_compile_pipe_with_backpressure(
    sock_path: &Path,
    req: &CompileRequest,
) -> std::io::Result<(tokio::net::windows::named_pipe::NamedPipeClient, Response)> {
    for attempt in 0..BACKPRESSURE_RETRY_LIMIT {
        let opened = open_windows_pipe_with_retry(sock_path).await?;
        let mut stream = opened.stream;
        let mut compile_req = req.clone();
        compile_req.ipc_busy_retries = compile_req
            .ipc_busy_retries
            .saturating_add(opened.busy_retries);
        let first = tokio::time::timeout(compile_reply_timeout(), async {
            write_frame_async(&mut stream, &Request::Compile(compile_req)).await?;
            read_frame_async(&mut stream).await
        })
        .await
        .map_err(|_| {
            windows_timeout_error("daemon IPC compile admission", compile_reply_timeout())
        })??;
        match first {
            Response::Backpressure { retry_after_ms } if attempt + 1 < BACKPRESSURE_RETRY_LIMIT => {
                let jitter_ms = (u64::from(attempt) * 11 + u64::from(std::process::id())) % 4;
                tokio::time::sleep(Duration::from_millis(u64::from(retry_after_ms) + jitter_ms))
                    .await;
            }
            response => return Ok((stream, response)),
        }
    }
    unreachable!("backpressure loop returns on the final response")
}

#[cfg(windows)]
fn run_windows_ipc<T, F>(operation: &'static str, timeout: Duration, f: F) -> Result<T, ClientError>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("soldr-daemon-client".to_string())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(ClientError::Io)?;

    match rx.recv_timeout(timeout) {
        Ok(result) => result.map_err(ClientError::from),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err(ClientError::Io(windows_timeout_error(operation, timeout)))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(ClientError::Io(
            std::io::Error::other(format!("{operation} worker exited without a result")),
        )),
    }
}

#[cfg(windows)]
fn submit_fire_and_forget_windows(sock_path: &Path, req: &Request) -> Result<(), ClientError> {
    use tokio::time::timeout;

    let sock_path = sock_path.to_path_buf();
    let req = req.clone();
    run_windows_ipc("daemon IPC hot-path write", HOT_PATH_TIMEOUT, move || {
        let runtime = windows_runtime()?;
        runtime.block_on(async move {
            let mut stream = open_windows_pipe_with_retry(&sock_path).await?.stream;
            timeout(HOT_PATH_TIMEOUT, write_frame_async(&mut stream, &req))
                .await
                .map_err(|_| {
                    windows_timeout_error("daemon IPC hot-path write", HOT_PATH_TIMEOUT)
                })??;
            Ok::<(), std::io::Error>(())
        })
    })
}

#[cfg(windows)]
fn submit_request_windows(sock_path: &Path, req: &Request) -> Result<Response, ClientError> {
    submit_request_windows_with_timeout(sock_path, req, REPLY_TIMEOUT)
}

#[cfg(windows)]
fn submit_request_windows_with_timeout(
    sock_path: &Path,
    req: &Request,
    deadline: Duration,
) -> Result<Response, ClientError> {
    submit_request_windows_with_timeout_and_version(
        sock_path,
        req,
        deadline,
        crate::daemon::protocol::PROTOCOL_VERSION,
    )
}

#[cfg(windows)]
fn submit_request_windows_with_timeout_and_version(
    sock_path: &Path,
    req: &Request,
    deadline: Duration,
    protocol_version: u32,
) -> Result<Response, ClientError> {
    use tokio::time::timeout;

    let sock_path = sock_path.to_path_buf();
    let req = req.clone();
    run_windows_ipc("daemon IPC request", deadline, move || {
        let runtime = windows_runtime()?;
        runtime.block_on(async move {
            let mut stream = open_windows_pipe_with_retry(&sock_path).await?.stream;
            timeout(deadline, async {
                write_frame_async_for_version(&mut stream, &req, protocol_version).await?;
                read_frame_async_for_version(&mut stream, protocol_version).await
            })
            .await
            .map_err(|_| windows_timeout_error("daemon IPC request", deadline))?
        })
    })
}

/// Windows variant of [`compile_streaming`]. Tunnels the chunks back
/// from a tokio runtime thread to the calling thread via a single
/// std::sync::mpsc channel; the calling thread drains the channel and
/// forwards bytes to the caller's `stdout` / `stderr` sinks (which
/// usually are `std::io::stdout()` and `std::io::stderr()` from the
/// wrapper). Keeping the user writers off the tokio thread sidesteps
/// Windows's blocking-IO-on-stdout quirks and matches the sync shape
/// the Unix branch uses.
#[cfg(windows)]
fn compile_streaming_windows<O, E>(
    sock_path: &Path,
    req: CompileRequest,
    stdout: &mut O,
    stderr: &mut E,
) -> Result<CompileDoneInfo, ClientError>
where
    O: Write,
    E: Write,
{
    use tokio::time::timeout;

    /// Frames forwarded from the IPC worker thread to the calling
    /// thread. `Done` carries the terminal metadata; `Err` short-
    /// circuits on protocol/io failure.
    enum StreamMsg {
        Stdout(Vec<u8>),
        Stderr(Vec<u8>),
        Done(CompileDoneInfo),
        Err(ClientError),
    }

    let sock_path = sock_path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel::<StreamMsg>();

    let worker = std::thread::Builder::new()
        .name("soldr-daemon-client-stream".into())
        .spawn(move || {
            let runtime = match windows_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(StreamMsg::Err(ClientError::Io(e)));
                    return;
                }
            };
            runtime.block_on(async move {
                let (mut stream, first_frame) = match open_compile_pipe_with_backpressure(
                    &sock_path, &req,
                )
                .await
                {
                    Ok(connection) => connection,
                    Err(e) => {
                        let _ = tx.send(StreamMsg::Err(ClientError::from(e)));
                        return;
                    }
                };
                let mut first_frame = Some(first_frame);
                loop {
                    let frame = match first_frame.take() {
                        Some(frame) => frame,
                        None => match timeout(
                            compile_reply_timeout(),
                            read_frame_async::<_, Response>(&mut stream),
                        )
                        .await
                        {
                            Ok(Ok(f)) => f,
                            Ok(Err(e)) => {
                                let _ = tx.send(StreamMsg::Err(ClientError::Io(e)));
                                return;
                            }
                            Err(_) => {
                                let _ = tx.send(StreamMsg::Err(ClientError::Io(
                                    windows_timeout_error(
                                        "daemon IPC compile read",
                                        compile_reply_timeout(),
                                    ),
                                )));
                                return;
                            }
                        },
                    };
                    match frame {
                        Response::CompileStdoutChunk(bytes) => {
                            tracing::debug!(
                                target: "soldr::client::compile_stream",
                                bytes = bytes.len(),
                                "stdout chunk received",
                            );
                            if tx.send(StreamMsg::Stdout(bytes)).is_err() {
                                return;
                            }
                        }
                        Response::CompileStderrChunk(bytes) => {
                            tracing::debug!(
                                target: "soldr::client::compile_stream",
                                bytes = bytes.len(),
                                "stderr chunk received",
                            );
                            if tx.send(StreamMsg::Stderr(bytes)).is_err() {
                                return;
                            }
                        }
                        Response::CompileDone {
                            exit_code,
                            cached,
                            cache_outcome,
                            compile_id,
                        } => {
                            tracing::debug!(
                                target: "soldr::client::compile_stream",
                                exit_code,
                                cached,
                                cache_outcome,
                                "compile done — streaming reply complete",
                            );
                            let _ = tx.send(StreamMsg::Done(CompileDoneInfo {
                                exit_code,
                                cached,
                                cache_outcome,
                                compile_id,
                            }));
                            return;
                        }
                        Response::Error(msg) => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Protocol(msg)));
                            return;
                        }
                        // soldr#1838 Phase 2. This is the transport #1837 was
                        // about: a wrapper connecting during the Windows
                        // graceful drain reached a latched-shut compile
                        // service. #1837 narrowed that window by releasing the
                        // pipe instance early; this handles a request that
                        // still lands inside it.
                        Response::Retiring => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Retiring));
                            return;
                        }
                        Response::Backpressure { retry_after_ms } => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Protocol(format!(
                                "daemon IPC admission remained backpressured after retry ({retry_after_ms}ms)"
                            ))));
                            return;
                        }
                        other => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Protocol(format!(
                                "unexpected compile stream frame: {other:?}"
                            ))));
                            return;
                        }
                    }
                }
            });
        })
        .map_err(ClientError::Io)?;

    // soldr#1838 bullet 4 — mirrors the unix arm. This consumer is the one
    // place on the Windows transport that sees both the chunks and the
    // worker's terminal error, so the slow-vs-wedged distinction is made
    // here rather than inside the worker thread.
    let started = std::time::Instant::now();
    let mut saw_output = false;
    // soldr#1838 Phase 1: the streaming phase used to run silent -- the
    // heartbeat on the request wait only covers the reply handshake. Publish
    // chunk arrivals so each beat can say whether output is still coming.
    let progress = super::wait_heartbeat::StreamProgress::new();
    let _stream_heartbeat = super::wait_heartbeat::WaitHeartbeat::start_streaming(
        "daemon compile stream",
        compile_reply_timeout(),
        Some(REPLY_TIMEOUT_ENV),
        std::sync::Arc::clone(&progress),
    );
    let result = loop {
        match rx.recv() {
            Ok(StreamMsg::Stdout(bytes)) => {
                saw_output = true;
                progress.record_chunk();
                stdout.write_all(&bytes).map_err(ClientError::Io)?;
            }
            Ok(StreamMsg::Stderr(bytes)) => {
                saw_output = true;
                progress.record_chunk();
                stderr.write_all(&bytes).map_err(ClientError::Io)?;
            }
            Ok(StreamMsg::Done(info)) => break Ok(info),
            Ok(StreamMsg::Err(ClientError::Io(err))) if is_deadline_error(&err) => {
                break Err(ClientError::CompileStalled {
                    saw_output,
                    elapsed: started.elapsed(),
                })
            }
            Ok(StreamMsg::Err(e)) => break Err(e),
            Err(_) => {
                break Err(ClientError::Io(std::io::Error::other(
                    "soldr-daemon-client-stream worker exited without a result",
                )))
            }
        }
    };
    // Best effort join — the worker has already pushed its final
    // message at this point, so this returns promptly.
    let _ = worker.join();
    result
}

/// Returns the well-known socket path the wrapper should use. Centralized
/// here so callers don't need to import `cache_lib` directly.
pub fn default_sock_path(paths: &SoldrPaths) -> PathBuf {
    #[cfg(unix)]
    {
        daemon_sock_path(paths)
    }
    #[cfg(windows)]
    {
        use crate::cache_lib::daemon_pipe_name;
        // soldr#1808: same infallible signature as `server_sock_path`, and
        // client and daemon must derive the identical name or they never
        // meet. Failing loudly beats dialing a name nothing is serving.
        PathBuf::from(format!(
            r"\\.\pipe\{}",
            daemon_pipe_name(paths).unwrap_or_else(|err| panic!("{err}"))
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(reply_timeout_defaults_to_30_min, {
        // Unset / empty / non-numeric / zero all fall back to the generous
        // default so a legitimate slow release compile is never cut off.
        let default = Duration::from_secs(DEFAULT_REPLY_TIMEOUT_SECS);
        assert_eq!(parse_reply_timeout(None), default);
        assert_eq!(parse_reply_timeout(Some("")), default);
        assert_eq!(parse_reply_timeout(Some("nope")), default);
        assert_eq!(parse_reply_timeout(Some("0")), default);
    });

    crate::timed_test!(reply_timeout_env_override_fails_fast, {
        // #1364: an operator can opt into a short fail-fast budget.
        assert_eq!(parse_reply_timeout(Some("30")), Duration::from_secs(30));
        assert_eq!(parse_reply_timeout(Some("  5 ")), Duration::from_secs(5));
    });
}
