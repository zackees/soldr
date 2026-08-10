//! The control-socket request loop (#704).
//!
//! Connects the two halves that shipped separately: the socket from the daemon
//! skeleton, and `ProbeOps` from the registration contract. Before this, the
//! daemon accepted connections and dropped them — every piece unit-tested,
//! nothing joined.
//!
//! # Connection close is the liveness signal
//!
//! [`Registry::drop_by_conn`] fires when a connection ends, on **every** exit
//! path — clean close, protocol error, or read failure. The heartbeat grace
//! only backstops SIGKILL, where no close ever arrives. A path that returns
//! without dropping would leave a registration claiming a process that is
//! gone, and the daemon would keep reporting it as `ARMED`.
//!
//! # Identity is verified here, not in `ProbeOps`
//!
//! `dispatch` is sans-io by design, so it takes a verdict rather than
//! computing one. Hashing the claimed executable and checking liveness are I/O,
//! so they happen at this boundary and the result is passed in.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use prost::Message as _;
use running_process::broker::protocol::framing::{read_frame_with_cap, write_frame};
use running_process::broker::server::PeerIdentity;
use running_process_probe::probe_diag::v1::ProbeEnvelope;

use crate::probe_ops::{IdentityVerdict, ProbeErrorCode, ProbeOps, ProbeReply, ProbeRequest};
use crate::registry::RegisterRequest;

const MAX_CONCURRENT_SYMBOLIZATIONS: usize = 2;
static ACTIVE_SYMBOLIZATIONS: AtomicUsize = AtomicUsize::new(0);

struct SymbolizationPermit;

impl SymbolizationPermit {
    fn try_acquire() -> Option<Self> {
        ACTIVE_SYMBOLIZATIONS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CONCURRENT_SYMBOLIZATIONS).then_some(active + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for SymbolizationPermit {
    fn drop(&mut self) {
        ACTIVE_SYMBOLIZATIONS.fetch_sub(1, Ordering::Release);
    }
}

/// Cap on one request frame.
///
/// Registration payloads are small; anything larger is malformed or hostile.
/// Deliberately far below the transport's 16 MiB ceiling so the bound is
/// enforced before the allocation, not after.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

// Wire translation lives in its own module; re-exported here because it is
// part of this module's public surface historically and callers (and the HTTP
// ingress) reach it through `serve::`.
pub use crate::wire_convert::{envelope_from_reply, request_from_envelope};

/// Hands out connection ids.
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate an id for a new connection.
pub fn next_conn_id() -> u64 {
    NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed)
}

/// Verify a registrant's claimed identity.
///
/// Three independent checks, all of which must hold before a process can be
/// armed: the executable still hashes to what was claimed, the boot id matches
/// this boot, and the process is actually alive. Any one failing means the
/// claim describes something other than the caller.
pub fn verify_identity(request: &RegisterRequest, connection_alive: bool) -> IdentityVerdict {
    let boot_matches = request.key.boot_id.is_empty()
        || running_process::broker::host_identity::current().boot_id == request.key.boot_id;

    let alive =
        running_process::broker::backend_lifecycle::verify_pid::process_is_alive(request.key.pid);

    let hash_matches = match running_process::broker::backend_lifecycle::identity::sha256_file(
        &request.exe_path,
    ) {
        Ok(actual) => actual == request.exe_sha256,
        // Unreadable executable cannot be verified, so it is not verified.
        Err(_) => false,
    };

    IdentityVerdict {
        verified: boot_matches && alive && hash_matches,
        connection_alive,
    }
}

/// Serve one connection until it closes.
///
/// Always drops the connection's registrations on the way out — see the module
/// docs on why that must hold for every exit path.
pub fn serve_connection<S: io::Read + io::Write>(
    stream: &mut S,
    ops: &ProbeOps,
    peer: &PeerIdentity,
    conn_id: u64,
) {
    // Includes clean EOF and oversize frames alike: either way this
    // connection is finished.
    while let Ok(bytes) = read_frame_with_cap(stream, MAX_REQUEST_BYTES) {
        let envelope = match ProbeEnvelope::decode(bytes.as_slice()) {
            Ok(e) => e,
            Err(_) => {
                let reply = ProbeReply::Refused {
                    code: ProbeErrorCode::MalformedRequest,
                    reason: "request did not decode as a ProbeEnvelope".into(),
                };
                let _ = write_reply(stream, 0, &reply);
                continue;
            }
        };
        let request_id = envelope.request_id;

        let Some(request) = request_from_envelope(envelope) else {
            let reply = ProbeReply::Refused {
                code: ProbeErrorCode::MalformedRequest,
                reason: "unsupported or incomplete request body".into(),
            };
            let _ = write_reply(stream, request_id, &reply);
            continue;
        };

        // Identity work is I/O, so it happens here rather than inside the
        // sans-io dispatcher.
        let verdict = match &request {
            ProbeRequest::Register(req) => verify_identity(req, true),
            _ => IdentityVerdict {
                verified: true,
                connection_alive: true,
            },
        };

        let reply = match request {
            ProbeRequest::CaptureResult(result) => finalize_capture_upload(ops, conn_id, result),
            other => ops.dispatch(other, peer, conn_id, verdict),
        };
        if write_reply(stream, request_id, &reply).is_err() {
            break;
        }
    }

    // Every exit path lands here. This is the daemon's primary death signal.
    ops.registry().drop_by_conn(conn_id);
}

fn finalize_capture_upload(
    ops: &ProbeOps,
    conn_id: u64,
    reply: running_process_probe::probe_diag::v1::CaptureReply,
) -> ProbeReply {
    let upload = match ops.capture_jobs().accept_upload(conn_id, reply) {
        Ok(upload) => upload,
        Err(reason) => {
            return ProbeReply::Refused {
                code: ProbeErrorCode::MalformedRequest,
                reason: reason.into(),
            };
        }
    };
    if upload.reply.error != 0 {
        return ProbeReply::Ack;
    }
    let worker = crate::symbolication::worker_path();
    finalize_accepted_upload(ops, upload, worker.as_deref())
}

#[cfg(test)]
fn finalize_capture_upload_with_worker(
    ops: &ProbeOps,
    conn_id: u64,
    reply: running_process_probe::probe_diag::v1::CaptureReply,
    worker: &std::path::Path,
) -> ProbeReply {
    let upload = match ops.capture_jobs().accept_upload(conn_id, reply) {
        Ok(upload) => upload,
        Err(reason) => {
            return ProbeReply::Refused {
                code: ProbeErrorCode::MalformedRequest,
                reason: reason.into(),
            };
        }
    };
    if upload.reply.error != 0 {
        return ProbeReply::Ack;
    }
    finalize_accepted_upload(ops, upload, Some(worker))
}

fn finalize_accepted_upload(
    ops: &ProbeOps,
    upload: crate::capture_jobs::CaptureUpload,
    worker: Option<&std::path::Path>,
) -> ProbeReply {
    let Some(_permit) = SymbolizationPermit::try_acquire() else {
        let failure =
            match discard_raw_capture(&upload.reply.artifact_path, upload.deadline_unix_ms) {
                Ok(()) => ReportFailure::internal("daemon symbolization capacity reached"),
                Err(error) => error,
            };
        ops.capture_jobs()
            .fail(&upload.job_id, failure.code, failure.detail);
        return ProbeReply::Ack;
    };
    let capture = match load_raw_capture(&upload.reply.artifact_path, upload.deadline_unix_ms) {
        Ok(capture) => capture,
        Err(error) => {
            ops.capture_jobs()
                .fail(&upload.job_id, error.code, error.detail);
            return ProbeReply::Ack;
        }
    };
    let Some(worker) = worker else {
        ops.capture_jobs().fail(
            &upload.job_id,
            5,
            crate::symbolication::WorkerError::NotFound.to_string(),
        );
        return ProbeReply::Ack;
    };
    match produce_symbol_reports(
        &upload.job_id,
        &capture.bytes,
        &capture.report_dir,
        worker,
        upload.deadline_unix_ms,
    ) {
        Ok(reports) => {
            if !ops
                .capture_jobs()
                .complete(&upload.job_id, reports.json.to_string_lossy().into_owned())
            {
                let _ = std::fs::remove_file(reports.json);
                let _ = std::fs::remove_file(reports.text);
            }
            ProbeReply::Ack
        }
        Err(error) => {
            ops.capture_jobs()
                .fail(&upload.job_id, error.code, error.detail);
            // The upload itself was accepted and its job now carries the
            // failure. Keep the healthy target connection armed; operators
            // observe this failure through GetJobStatus.
            ProbeReply::Ack
        }
    }
}

struct SymbolReports {
    json: PathBuf,
    text: PathBuf,
}

struct RawArtifactCleanup(PathBuf);

impl Drop for RawArtifactCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct ReportFailure {
    code: i32,
    detail: String,
}

impl ReportFailure {
    fn internal(detail: impl Into<String>) -> Self {
        Self {
            code: 5,
            detail: detail.into(),
        }
    }

    fn deadline(detail: impl Into<String>) -> Self {
        Self {
            code: 4,
            detail: detail.into(),
        }
    }
}

impl From<String> for ReportFailure {
    fn from(detail: String) -> Self {
        Self::internal(detail)
    }
}

impl From<&str> for ReportFailure {
    fn from(detail: &str) -> Self {
        Self::internal(detail)
    }
}

struct LoadedCapture {
    bytes: Vec<u8>,
    report_dir: PathBuf,
    _cleanup: RawArtifactCleanup,
}

struct OpenedRawArtifact {
    file: std::fs::File,
    len: u64,
    report_dir: PathBuf,
    cleanup: RawArtifactCleanup,
}

fn open_raw_artifact(
    raw_path: &str,
    deadline_unix_ms: u64,
) -> Result<OpenedRawArtifact, ReportFailure> {
    const MAX_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;
    let _ = remaining_worker_time(deadline_unix_ms)?;
    let path = PathBuf::from(raw_path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "capture artifact has no valid filename".to_string())?;
    if !file_name.starts_with("rp-probe-capture-") || !file_name.ends_with(".json") {
        return Err("capture artifact is not a probe-owned temporary file".into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "capture artifact has no parent directory".to_string())?;
    let expected_parent = std::env::temp_dir()
        .canonicalize()
        .map_err(|error| format!("cannot resolve temp directory: {error}"))?;
    let actual_parent = parent
        .canonicalize()
        .map_err(|error| format!("cannot resolve capture directory: {error}"))?;
    if actual_parent != expected_parent {
        return Err("capture artifact is outside the owner-local temp directory".into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // FILE_FLAG_OPEN_REPARSE_POINT: inspect the named object itself, not
        // any symlink/junction target selected after validation.
        options.custom_flags(0x0020_0000);
    }
    let file = options
        .open(&path)
        .map_err(|error| format!("cannot open capture artifact: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect capture artifact: {error}"))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("capture artifact is a reparse point".into());
        }
    }
    if !metadata.is_file() {
        return Err("capture artifact is not a regular file".into());
    }
    // From this point the named object has passed the daemon-owned path and
    // no-follow handle checks. Consume it exactly once even if parsing or
    // worker execution fails, so raw stack data does not accumulate in temp.
    let cleanup = RawArtifactCleanup(path.clone());
    if metadata.len() > MAX_CAPTURE_BYTES {
        return Err(format!(
            "capture artifact is not a bounded regular file ({} bytes)",
            metadata.len()
        )
        .into());
    }
    Ok(OpenedRawArtifact {
        file,
        len: metadata.len(),
        report_dir: expected_parent,
        cleanup,
    })
}

fn discard_raw_capture(raw_path: &str, deadline_unix_ms: u64) -> Result<(), ReportFailure> {
    let _artifact = open_raw_artifact(raw_path, deadline_unix_ms)?;
    Ok(())
}

fn load_raw_capture(raw_path: &str, deadline_unix_ms: u64) -> Result<LoadedCapture, ReportFailure> {
    const MAX_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;
    let artifact = open_raw_artifact(raw_path, deadline_unix_ms)?;
    let mut capture = Vec::with_capacity(artifact.len as usize);
    let mut limited = io::Read::take(artifact.file, MAX_CAPTURE_BYTES + 1);
    io::Read::read_to_end(&mut limited, &mut capture)
        .map_err(|error| format!("cannot read capture artifact: {error}"))?;
    if capture.len() as u64 > MAX_CAPTURE_BYTES {
        return Err("capture artifact grew beyond the 64 MiB limit".into());
    }
    Ok(LoadedCapture {
        bytes: capture,
        report_dir: artifact.report_dir,
        _cleanup: artifact.cleanup,
    })
}

fn produce_symbol_reports(
    job_id: &str,
    capture: &[u8],
    report_dir: &std::path::Path,
    worker: &std::path::Path,
    deadline_unix_ms: u64,
) -> Result<SymbolReports, ReportFailure> {
    let budget = remaining_worker_time(deadline_unix_ms)?;
    let json = crate::symbolication::symbolize_with_worker_at(worker, capture, budget.timeout)
        .map_err(|error| classify_worker_failure(error, budget.deadline_limited))?;
    let budget = remaining_worker_time(deadline_unix_ms)?;
    let text = crate::symbolication::symbolize_with_worker_at_text(worker, capture, budget.timeout)
        .map_err(|error| classify_worker_failure(error, budget.deadline_limited))?;

    let json_path = report_dir.join(format!("rp-probe-report-{job_id}.symbolized.json"));
    let text_path = report_dir.join(format!("rp-probe-report-{job_id}.symbolized.txt"));
    write_new(&json_path, json.as_bytes())
        .map_err(|error| format!("cannot write JSON report: {error}"))?;
    if let Err(error) = write_new(&text_path, text.as_bytes()) {
        let _ = std::fs::remove_file(&json_path);
        return Err(format!("cannot write text report: {error}").into());
    }
    Ok(SymbolReports {
        json: json_path,
        text: text_path,
    })
}

#[derive(Clone, Copy)]
struct WorkerBudget {
    timeout: std::time::Duration,
    deadline_limited: bool,
}

fn remaining_worker_time(deadline_unix_ms: u64) -> Result<WorkerBudget, ReportFailure> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let remaining_ms = deadline_unix_ms.saturating_sub(now);
    if remaining_ms == 0 {
        return Err(ReportFailure::deadline("capture deadline elapsed"));
    }
    let remaining = std::time::Duration::from_millis(remaining_ms);
    Ok(WorkerBudget {
        timeout: remaining.min(crate::symbolication::DEFAULT_WORKER_TIMEOUT),
        deadline_limited: remaining <= crate::symbolication::DEFAULT_WORKER_TIMEOUT,
    })
}

fn classify_worker_failure(
    error: crate::symbolication::WorkerError,
    deadline_limited: bool,
) -> ReportFailure {
    if deadline_limited && matches!(error, crate::symbolication::WorkerError::Timeout(_)) {
        ReportFailure::deadline(error.to_string())
    } else {
        ReportFailure::internal(error.to_string())
    }
}

fn write_new(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    use io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let result = file.write_all(bytes).and_then(|()| file.flush());
    if result.is_err() {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    result
}

fn write_reply<S: io::Write>(
    stream: &mut S,
    request_id: u64,
    reply: &ProbeReply,
) -> io::Result<()> {
    let envelope = envelope_from_reply(request_id, reply);
    write_frame(stream, &envelope.encode_to_vec()).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(())
}

/// Build the ops core for a daemon owned by the current user.
///
/// The registry's owner and the peer policy MUST come from the same source.
/// `ProbeOps` compares peers against the policy, and `Registry::begin_register`
/// compares them against its own `owner` string — if those are expressed
/// differently (a raw uid/SID versus, say, a SID *hash* used for endpoint
/// naming), one of the two checks rejects everything while the other passes.
/// Both are derived here from `PeerCredentialPolicy::current_user()`, which is
/// also what `peer_identity_from_stream` reports, so all three agree.
pub fn build_ops() -> io::Result<Arc<ProbeOps>> {
    let policy = running_process::broker::server::PeerCredentialPolicy::current_user()
        .ok_or_else(|| io::Error::other("cannot resolve the current user for the owner policy"))?;

    let owner = match &policy {
        running_process::broker::server::PeerCredentialPolicy::OwnerOnly { uid_or_sid } => {
            uid_or_sid.clone()
        }
        #[allow(unreachable_patterns)]
        _ => return Err(io::Error::other("owner policy is not owner-scoped")),
    };

    let mut ops = ProbeOps::new(Arc::new(crate::registry::Registry::new(owner)), policy);

    // Best-effort. A crash store that will not open (full or read-only home,
    // a stale database from a newer daemon) must not stop the daemon from
    // serving registrations, captures, and `ps` — those are the surfaces
    // processes depend on to stay observable at all. Crash queries then
    // refuse with a reason instead of the daemon refusing to exist.
    let artifacts_dir = crate::crash_store::default_artifacts_dir();
    match crate::crash_store::CrashStore::open(&artifacts_dir.join("crashes.db"), &artifacts_dir) {
        Ok(store) => ops = ops.with_crash_store(Arc::new(store)),
        Err(error) => {
            eprintln!("rpprobed: crash history unavailable: {error}");
        }
    }

    Ok(Arc::new(ops))
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::broker::server::PeerCredentialPolicy;
    use running_process_probe::probe_diag::v1 as wire;
    use running_process_probe::probe_diag::v1::{
        probe_envelope::Body, RegisterProcess, RegistrationStatus,
    };

    use crate::registry::{AllowPolicy, Disclosure, ProcessKey, Runtime};
    use crate::wire_convert::key_from_proto;

    const OWNER: &str = "owner-uid";

    fn ops() -> ProbeOps {
        ProbeOps::new(
            Arc::new(crate::registry::Registry::new(OWNER.into())),
            PeerCredentialPolicy::OwnerOnly {
                uid_or_sid: OWNER.into(),
            },
        )
    }

    fn peer() -> PeerIdentity {
        PeerIdentity {
            pid: std::process::id(),
            uid_or_sid: OWNER.into(),
        }
    }

    /// Register this very process, so identity verification can actually
    /// succeed against a real executable.
    fn self_register_envelope(nonce: u8, request_id: u64) -> ProbeEnvelope {
        let exe = std::env::current_exe().expect("current exe");
        let sha = running_process::broker::backend_lifecycle::identity::sha256_file(&exe)
            .expect("hash self");
        ProbeEnvelope {
            wire_version: 1,
            request_id,
            deadline_unix_ms: 0,
            body: Some(Body::Register(RegisterProcess {
                key: Some(wire::ProcessKey {
                    pid: u64::from(std::process::id()),
                    start_time: Some(1_700_000_000_000),
                    boot_id: Some(running_process::broker::host_identity::current().boot_id),
                }),
                exe_path: exe.to_string_lossy().into_owned(),
                exe_sha256: sha.to_vec(),
                app_class: "test".into(),
                registration_nonce: vec![nonce; 32],
                ..Default::default()
            })),
        }
    }

    /// A heartbeat for this process, carrying `request_id`.
    fn heartbeat_envelope(request_id: u64) -> ProbeEnvelope {
        ProbeEnvelope {
            wire_version: 1,
            request_id,
            deadline_unix_ms: 0,
            body: Some(Body::Heartbeat(wire::Heartbeat {
                key: Some(wire::ProcessKey {
                    pid: u64::from(std::process::id()),
                    start_time: Some(1_700_000_000_000),
                    boot_id: Some(running_process::broker::host_identity::current().boot_id),
                }),
            })),
        }
    }

    /// Drive `serve_connection` over an in-memory duplex.
    fn serve_bytes(ops: &ProbeOps, requests: &[ProbeEnvelope], conn_id: u64) -> Vec<ProbeEnvelope> {
        let mut input = Vec::new();
        for env in requests {
            write_frame(&mut input, &env.encode_to_vec()).unwrap();
        }

        struct Duplex {
            read: std::io::Cursor<Vec<u8>>,
            written: Vec<u8>,
        }
        impl io::Read for Duplex {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.read.read(buf)
            }
        }
        impl io::Write for Duplex {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.written.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut duplex = Duplex {
            read: std::io::Cursor::new(input),
            written: Vec::new(),
        };
        serve_connection(&mut duplex, ops, &peer(), conn_id);

        let mut replies = Vec::new();
        let mut cursor = std::io::Cursor::new(duplex.written);
        while let Ok(frame) = read_frame_with_cap(&mut cursor, MAX_REQUEST_BYTES) {
            if let Ok(env) = ProbeEnvelope::decode(frame.as_slice()) {
                replies.push(env);
            }
        }
        replies
    }

    fn status(env: &ProbeEnvelope) -> RegistrationStatus {
        match env.body.clone() {
            Some(Body::RegistrationStatus(s)) => s,
            other => panic!("expected RegistrationStatus, got {other:?}"),
        }
    }

    #[test]
    fn registering_this_process_reaches_armed() {
        let ops = ops();
        let replies = serve_bytes(&ops, &[self_register_envelope(1, 7)], 1);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].request_id, 7, "request_id must be echoed");
        assert_eq!(status(&replies[0]).state, 2, "2 == ARMED");
    }

    #[test]
    fn live_query_round_trips_over_the_real_framed_ingress() {
        let ops = ops();
        let mut registration = self_register_envelope(0x64, 1);
        let Some(Body::Register(register)) = registration.body.as_mut() else {
            panic!("expected registration");
        };
        register.app_name = "query-fixture".into();
        register.disclosed_cwd = "C:/query-fixture".into();
        register.allow_policy = Some(wire::AllowPolicy {
            allow_all_ops: true,
            env_allowlist: vec!["VISIBLE".into()],
        });
        register.disclosure = Some(wire::Disclosure {
            expose_exe_path: false,
            expose_cmdline: false,
            expose_env_names: true,
        });
        register
            .disclosed_env
            .insert("VISIBLE".into(), "yes".into());

        let matching = ProbeEnvelope {
            wire_version: 1,
            request_id: 2,
            deadline_unix_ms: 0,
            body: Some(Body::ProcessQuery(wire::ProcessQuery {
                name_glob: "*".into(),
                cwd_regex: "query-fixture$".into(),
                app_class: "test".into(),
                include_env: true,
                limit: 5,
                env: vec![wire::EnvMatch {
                    key: "VISIBLE".into(),
                    value_exact: Some("yes".into()),
                    ..Default::default()
                }],
                ..Default::default()
            })),
        };
        let hidden = ProbeEnvelope {
            wire_version: 1,
            request_id: 3,
            deadline_unix_ms: 0,
            body: Some(Body::ProcessQuery(wire::ProcessQuery {
                limit: 5,
                env: vec![wire::EnvMatch {
                    key: "SECRET".into(),
                    ..Default::default()
                }],
                ..Default::default()
            })),
        };

        let replies = serve_bytes(&ops, &[registration, matching, hidden], 64);
        let Some(Body::ProcessQueryReply(matching)) = replies[1].body.as_ref() else {
            panic!("expected process query reply");
        };
        assert_eq!(matching.processes.len(), 1);
        let process = &matching.processes[0];
        assert!(process.registered);
        assert_eq!(process.cwd, "C:/query-fixture");
        assert_eq!(process.env.get("VISIBLE").map(String::as_str), Some("yes"));
        assert_eq!(process.env_names, ["VISIBLE"]);

        let Some(Body::ProcessQueryReply(hidden)) = replies[2].body.as_ref() else {
            panic!("expected process query reply");
        };
        assert!(
            hidden.processes.is_empty(),
            "a non-allowlisted key must be invisible to both filtering and results"
        );
    }

    /// Every reply to a decodable request must echo that request's id, across
    /// a whole connection and whether the request succeeded or was refused.
    ///
    /// The client verifies this on its side and treats a mismatch as a dead
    /// connection (`ClientError::Desync`). If the daemon ever stopped echoing
    /// — including on the refusal paths, which are easy to overlook — every
    /// client would drop its connection and re-register in a loop. Asserting
    /// it on only the happy path would not catch that.
    #[test]
    fn every_reply_echoes_the_id_of_the_request_it_answers() {
        let ops = ops();
        // Distinct, non-sequential ids so an off-by-one or an index-based
        // reply would be visible rather than coincidentally correct.
        let requests = vec![
            self_register_envelope(1, 100),
            // Same nonce again: refused as a replay, and still has to echo.
            self_register_envelope(1, 205),
            heartbeat_envelope(311),
        ];
        let sent: Vec<u64> = requests.iter().map(|r| r.request_id).collect();

        let replies = serve_bytes(&ops, &requests, 1);

        assert_eq!(
            replies.len(),
            sent.len(),
            "every request must draw exactly one reply"
        );
        let echoed: Vec<u64> = replies.iter().map(|r| r.request_id).collect();
        assert_eq!(echoed, sent, "replies must echo their request ids in order");
    }

    /// The contract that makes the daemon's liveness model work.
    #[test]
    fn closing_the_connection_drops_the_registration_immediately() {
        let ops = ops();
        serve_bytes(&ops, &[self_register_envelope(2, 1)], 42);
        assert!(
            ops.registry().is_empty(),
            "connection close must drop registrations at once, not after the \
             heartbeat grace"
        );
    }

    #[test]
    fn a_malformed_frame_is_refused_not_dropped() {
        let ops = ops();
        let mut input = Vec::new();
        write_frame(&mut input, b"not a protobuf at all").unwrap();

        struct R(std::io::Cursor<Vec<u8>>, Vec<u8>);
        impl io::Read for R {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                self.0.read(b)
            }
        }
        impl io::Write for R {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.1.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut s = R(std::io::Cursor::new(input), Vec::new());
        serve_connection(&mut s, &ops, &peer(), 1);

        let mut cursor = std::io::Cursor::new(s.1);
        let frame = read_frame_with_cap(&mut cursor, MAX_REQUEST_BYTES)
            .expect("a refusal must be written, not the connection silently closed");
        let env = ProbeEnvelope::decode(frame.as_slice()).unwrap();
        assert_eq!(status(&env).state, 3, "3 == DROPPED/refused");
    }

    #[test]
    fn a_foreign_peer_is_refused() {
        let ops = ops();
        let stranger = PeerIdentity {
            pid: 1,
            uid_or_sid: "someone-else".into(),
        };
        let mut input = Vec::new();
        write_frame(&mut input, &self_register_envelope(3, 1).encode_to_vec()).unwrap();

        struct R(std::io::Cursor<Vec<u8>>, Vec<u8>);
        impl io::Read for R {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                self.0.read(b)
            }
        }
        impl io::Write for R {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.1.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut s = R(std::io::Cursor::new(input), Vec::new());
        serve_connection(&mut s, &ops, &stranger, 1);

        assert!(
            ops.registry().is_empty(),
            "a foreign peer must create nothing"
        );
    }

    /// A key without a start time cannot survive PID reuse.
    #[test]
    fn a_key_without_a_start_time_is_refused() {
        let key = wire::ProcessKey {
            pid: 10,
            start_time: None,
            boot_id: Some("b".into()),
        };
        assert!(
            key_from_proto(key).is_none(),
            "pid-only identity would silently alias across PID reuse"
        );
    }

    /// The registry owner and the peer policy must be expressed identically.
    ///
    /// They are compared against the same socket-derived `PeerIdentity` by two
    /// different code paths (`ProbeOps::dispatch` via the policy,
    /// `Registry::begin_register` via the owner string). If one held a raw
    /// uid/SID and the other a SID *hash*, registration would be rejected
    /// unconditionally while the policy check passed — a mismatch that only
    /// appears once real credentials are read off the socket.
    #[test]
    fn registry_owner_matches_the_peer_policy() {
        let ops = build_ops().expect("build ops");

        let policy_owner = match ops.owner_policy_for_test() {
            PeerCredentialPolicy::OwnerOnly { uid_or_sid } => uid_or_sid,
            other => panic!("expected OwnerOnly, got {other:?}"),
        };

        // A peer reporting exactly the policy's identity must be accepted by
        // the registry too, not just by the policy.
        let peer = PeerIdentity {
            pid: std::process::id(),
            uid_or_sid: policy_owner,
        };
        let reply = ops.dispatch(
            ProbeRequest::Heartbeat(ProcessKey {
                pid: 1,
                started_at_unix_ms: 1,
                boot_id: String::new(),
            }),
            &peer,
            1,
            IdentityVerdict {
                verified: true,
                connection_alive: true,
            },
        );
        // NotRegistered is the right refusal here (no such key). PeerRejected
        // would mean the owner strings disagree.
        match reply {
            ProbeReply::Refused { code, .. } => assert_eq!(
                code,
                ProbeErrorCode::NotRegistered,
                "owner mismatch: the registry rejected the policy's own identity"
            ),
            other => panic!("unexpected reply {other:?}"),
        }
    }

    #[test]
    fn identity_verification_rejects_a_wrong_hash() {
        let exe = std::env::current_exe().unwrap();
        let request = RegisterRequest {
            key: ProcessKey {
                pid: std::process::id(),
                started_at_unix_ms: 1,
                boot_id: String::new(),
            },
            exe_path: exe,
            // Deliberately not the real hash.
            exe_sha256: [0xAB; 32],
            app_class: "x".into(),
            app_name: "x".into(),
            app_version: "1".into(),
            instance_name: String::new(),
            allow_policy: AllowPolicy::default(),
            disclosure: Disclosure::default(),
            disclosed_cwd: None,
            disclosed_env: Default::default(),
            nonce: [1u8; 32],
            supported_ops: Vec::new(),
            runtime: Runtime::Native,
            symbol_source: 2,
            symbol_manifest_path: None,
            symbol_paths: Vec::new(),
        };
        assert!(
            !verify_identity(&request, true).verified,
            "a mismatched executable hash must not verify"
        );
    }

    /// A capture request must be recognised, not fall through to the
    /// unsupported-body path.
    #[test]
    fn a_capture_request_is_decoded() {
        let envelope = ProbeEnvelope {
            wire_version: 1,
            request_id: 1,
            deadline_unix_ms: 0,
            body: Some(Body::CaptureStack(wire::CaptureStackRequest {
                key: Some(wire::ProcessKey {
                    pid: 4242,
                    start_time: Some(1_700_000_000_000),
                    boot_id: Some("boot".into()),
                }),
                ..Default::default()
            })),
        };
        match request_from_envelope(envelope) {
            Some(ProbeRequest::CaptureStack { key, .. }) => {
                assert_eq!(key.pid, 4242);
                assert_eq!(key.started_at_unix_ms, 1_700_000_000_000);
            }
            other => panic!("expected CaptureStack, got {other:?}"),
        }
    }

    /// A capture request without a start time must be refused for the same
    /// reason registration is: a key that cannot survive PID reuse would let
    /// a capture target whatever process now holds that pid.
    #[test]
    fn a_capture_request_without_a_start_time_is_rejected() {
        let envelope = ProbeEnvelope {
            wire_version: 1,
            request_id: 1,
            deadline_unix_ms: 0,
            body: Some(Body::CaptureStack(wire::CaptureStackRequest {
                key: Some(wire::ProcessKey {
                    pid: 4242,
                    start_time: None,
                    boot_id: Some("boot".into()),
                }),
                ..Default::default()
            })),
        };
        assert!(request_from_envelope(envelope).is_none());
    }

    /// A declared runtime must survive the wire and land in the registry.
    ///
    /// Goes through the real proto decode and the real `dispatch`, then reads
    /// the stored entry back, so a break anywhere in the chain — proto field,
    /// `from_proto`, `RegisterRequest`, or the `RegEntry` construction — fails
    /// here.
    ///
    /// `dispatch` is driven directly rather than through `serve_connection`
    /// because that function drops the connection's registrations on its way
    /// out, which is exactly the behavior we want everywhere else.
    fn stored_runtime_for(wire_runtime: i32, nonce: u8) -> Runtime {
        let registry = Arc::new(crate::registry::Registry::new(OWNER.into()));
        let ops = ProbeOps::new(
            Arc::clone(&registry),
            PeerCredentialPolicy::OwnerOnly {
                uid_or_sid: OWNER.into(),
            },
        );

        let mut envelope = self_register_envelope(nonce, 1);
        let Some(Body::Register(req)) = envelope.body.as_mut() else {
            panic!("expected a register body");
        };
        req.runtime = wire_runtime;

        let request = request_from_envelope(envelope).expect("decodes");
        let ProbeRequest::Register(reg) = &request else {
            panic!("expected a register request");
        };
        let verdict = verify_identity(reg, true);
        let key = reg.key.clone();

        let reply = ops.dispatch(request, &peer(), 1, verdict);
        assert!(
            !matches!(reply, ProbeReply::Refused { .. }),
            "registration was refused: {reply:?}"
        );

        registry
            .get(&key)
            .expect("registration should be stored")
            .runtime
    }

    #[test]
    fn a_declared_python_runtime_reaches_the_registry() {
        assert_eq!(
            stored_runtime_for(wire::Runtime::Python as i32, 0x51),
            Runtime::Python,
            "the daemon must record the runtime the client declared"
        );
    }

    /// Without this, the test above would pass no matter what was sent.
    #[test]
    fn an_undeclared_runtime_is_not_python() {
        assert_eq!(
            stored_runtime_for(wire::Runtime::Unspecified as i32, 0x52),
            Runtime::Unspecified
        );
    }

    /// A native declaration is distinguishable from no declaration at all.
    #[test]
    fn a_declared_native_runtime_is_recorded_as_native() {
        assert_eq!(
            stored_runtime_for(wire::Runtime::Native as i32, 0x54),
            Runtime::Native
        );
    }

    /// A runtime this daemon does not know must not cost the registration.
    ///
    /// The proto reserves 3..15 for future runtimes. An older daemon meeting a
    /// newer client should still register it and simply skip runtime-specific
    /// handling.
    #[test]
    fn an_unknown_runtime_still_registers() {
        assert_eq!(stored_runtime_for(9, 0x53), Runtime::Unspecified);
    }

    /// Full #637 daemon boundary: a leased target artifact is path-checked,
    /// symbolized by real disposable workers into both formats, and exposed as
    /// a completed asynchronous job.
    #[test]
    fn leased_capture_becomes_json_and_text_job_artifacts() {
        let mut worker = std::env::current_exe().expect("test executable");
        worker.pop();
        if worker.ends_with("deps") {
            worker.pop();
        }
        worker.push(format!(
            "running-process-probe-worker{}",
            std::env::consts::EXE_SUFFIX
        ));
        if !worker.is_file() {
            assert!(
                std::env::var_os("GITHUB_ACTIONS").is_none(),
                "worker binary missing at {} in CI",
                worker.display()
            );
            eprintln!("skipping: worker binary not built");
            return;
        }

        let ops = ops();
        let key = ProcessKey {
            pid: std::process::id(),
            started_at_unix_ms: 17,
            boot_id: "boot".into(),
        };
        let receipt = ops
            .capture_jobs()
            .enqueue(key.clone(), 64, 0, 0)
            .expect("enqueued");
        ops.capture_jobs().lease(&key, 71).expect("lease");
        let raw_path = std::env::temp_dir().join(format!(
            "rp-probe-capture-test-{}-{}.json",
            std::process::id(),
            receipt.job_id
        ));
        std::fs::write(
            &raw_path,
            br#"{"format":"cooperative_frames","modules":[{"name":"fixture.dll"}],
                "threads":[{"os_tid":7,"frames":[{"module_index":0,"relative_address":16}],
                "py_frames":[{"file":"fixture.py","line":3,"func":"handler"}]}]}"#,
        )
        .expect("raw capture");

        let reply = finalize_capture_upload_with_worker(
            &ops,
            71,
            wire::CaptureReply {
                artifact_path: raw_path.to_string_lossy().into_owned(),
                threads_captured: 1,
                ..Default::default()
            },
            &worker,
        );
        assert_eq!(reply, ProbeReply::Ack);
        let status = ops
            .capture_jobs()
            .status(&receipt.job_id)
            .expect("job status");
        assert_eq!(status.state, wire::job_status::State::Complete as i32);
        let json_path = PathBuf::from(&status.artifact_path);
        let text_path =
            std::env::temp_dir().join(format!("rp-probe-report-{}.symbolized.txt", receipt.job_id));
        let json = std::fs::read_to_string(&json_path).expect("JSON report");
        let text = std::fs::read_to_string(&text_path).expect("text report");
        assert!(json.contains("fixture.dll"), "{json}");
        assert!(json.contains("handler"), "{json}");
        assert!(text.contains("fixture.dll"), "{text}");
        assert!(text.contains("handler"), "{text}");
        assert!(
            !raw_path.exists(),
            "the daemon must consume the sensitive raw artifact"
        );

        for path in [&raw_path, &json_path, &text_path] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn accepted_job_failure_is_acked_without_poisoning_the_target_connection() {
        let ops = ops();
        let key = ProcessKey {
            pid: 91,
            started_at_unix_ms: 17,
            boot_id: "boot".into(),
        };
        let receipt = ops
            .capture_jobs()
            .enqueue(key.clone(), 64, 0, 0)
            .expect("enqueued");
        ops.capture_jobs().lease(&key, 71).expect("lease");
        let missing =
            std::env::temp_dir().join(format!("rp-probe-capture-missing-{}.json", receipt.job_id));
        let reply = finalize_capture_upload_with_worker(
            &ops,
            71,
            wire::CaptureReply {
                artifact_path: missing.to_string_lossy().into_owned(),
                ..Default::default()
            },
            std::path::Path::new("unused-worker"),
        );
        assert_eq!(reply, ProbeReply::Ack);
        assert_eq!(
            ops.capture_jobs().status(&receipt.job_id).unwrap().state,
            wire::job_status::State::Failed as i32
        );

        ops.capture_jobs()
            .enqueue(key.clone(), 64, 0, 0)
            .expect("a failed job must release capacity");
        assert!(
            ops.capture_jobs().lease(&key, 71).is_some(),
            "the same target connection remains usable"
        );
    }

    #[test]
    fn worker_timeout_uses_the_deadline_code_only_when_job_budget_limited() {
        let deadline = classify_worker_failure(
            crate::symbolication::WorkerError::Timeout(std::time::Duration::from_secs(1)),
            true,
        );
        assert_eq!(deadline.code, 4);
        let safety_cap = classify_worker_failure(
            crate::symbolication::WorkerError::Timeout(
                crate::symbolication::DEFAULT_WORKER_TIMEOUT,
            ),
            false,
        );
        assert_eq!(safety_cap.code, 5);
    }

    #[test]
    fn missing_worker_still_consumes_an_accepted_raw_artifact() {
        let ops = ops();
        let key = ProcessKey {
            pid: 92,
            started_at_unix_ms: 17,
            boot_id: "boot".into(),
        };
        let receipt = ops
            .capture_jobs()
            .enqueue(key.clone(), 64, 0, 0)
            .expect("enqueued");
        ops.capture_jobs().lease(&key, 72).expect("lease");
        let raw = std::env::temp_dir().join(format!(
            "rp-probe-capture-worker-missing-{}.json",
            receipt.job_id
        ));
        std::fs::write(&raw, b"{}").unwrap();
        let upload = ops
            .capture_jobs()
            .accept_upload(
                72,
                wire::CaptureReply {
                    artifact_path: raw.to_string_lossy().into_owned(),
                    ..Default::default()
                },
            )
            .expect("accepted");

        assert_eq!(
            finalize_accepted_upload(&ops, upload, None),
            ProbeReply::Ack
        );
        assert!(!raw.exists());
        assert_eq!(
            ops.capture_jobs().status(&receipt.job_id).unwrap().state,
            wire::job_status::State::Failed as i32
        );
    }
}
