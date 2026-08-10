//! Wire ↔ domain translation for the probe control socket.
//!
//! Split out of [`crate::serve`] so the request loop stays readable next to
//! the schema mapping it depends on, and so the file stays under the repo's
//! per-file size guard. Everything here is a pure function of its argument —
//! no sockets, no filesystem — which is what lets the serve tests build
//! envelopes directly, and what will let the HTTP ingress reuse this mapping
//! rather than growing a second one that drifts from it.

use std::num::NonZeroU32;
use std::path::PathBuf;

use running_process_probe::probe_diag::v1 as wire;
use running_process_probe::probe_diag::v1::{
    probe_envelope::Body, CrashQueryReply, CrashStatsReply, ProbeEnvelope, ProcessQueryReply,
    RegisterProcess, RegistrationStatus,
};

use crate::crash_query::CrashFilter;
use crate::probe_ops::{ProbeErrorCode, ProbeReply, ProbeRequest};
use crate::registry::{AllowPolicy, Disclosure, ProcessKey, RegisterRequest, Runtime};

/// Translate a wire `ProbeEnvelope` into a domain request.
///
/// Returns `None` for bodies this daemon does not serve, which the caller
/// answers with a structured refusal rather than closing the connection — a
/// client speaking a newer schema should get a reply it can interpret.
pub fn request_from_envelope(envelope: ProbeEnvelope) -> Option<ProbeRequest> {
    let deadline_unix_ms = envelope.deadline_unix_ms;
    match envelope.body? {
        Body::Register(req) => Some(ProbeRequest::Register(Box::new(register_from_proto(req)?))),
        Body::Heartbeat(hb) => Some(ProbeRequest::Heartbeat(key_from_proto(hb.key?)?)),
        Body::Unregister(un) => Some(ProbeRequest::Unregister(key_from_proto(un.key?)?)),
        // Recognised even though forwarding is unimplemented, so the caller
        // gets a reason about the *target* rather than "unsupported body",
        // which would be indistinguishable from a message this daemon has
        // never heard of.
        Body::CaptureStack(req) => Some(ProbeRequest::CaptureStack {
            key: key_from_proto(req.key?)?,
            max_depth: req.max_depth,
            thread_filter: req.thread_filter,
            deadline_unix_ms,
        }),
        Body::CaptureReply(reply) => Some(ProbeRequest::CaptureResult(reply)),
        Body::JobStatusReq(request) => Some(ProbeRequest::GetJobStatus(request.job_id)),
        Body::ProcessQuery(query) => Some(ProbeRequest::Query(Box::new(
            crate::query::ProcessQuery::from_proto(query).ok()?,
        ))),
        Body::CrashQuery(query) => {
            let limit = NonZeroU32::new(query.limit)?;
            Some(ProbeRequest::QueryCrashes {
                filter: Box::new(crash_filter_from_proto(query)),
                limit,
            })
        }
        // Stats carry the same filter but no limit: the result is bounded by
        // the number of distinct signatures, not by the number of crashes.
        Body::CrashStatsQuery(request) => Some(ProbeRequest::CrashStats(Box::new(
            crash_filter_from_proto(request.filter.unwrap_or_default()),
        ))),
        Body::ListProcesses(list) => Some(ProbeRequest::Query(Box::new(
            crate::query::ProcessQuery::from_proto(
                running_process_probe::probe_diag::v1::ProcessQuery {
                    limit: list.limit,
                    include_env: list.include_env,
                    ..Default::default()
                },
            )
            .ok()?,
        ))),
        _ => None,
    }
}

pub(crate) fn key_from_proto(
    key: running_process_probe::probe_diag::v1::ProcessKey,
) -> Option<ProcessKey> {
    Some(ProcessKey {
        pid: u32::try_from(key.pid).ok()?,
        // A key without a start time cannot survive PID reuse, so refuse it
        // rather than register an identity that may silently alias.
        started_at_unix_ms: key.start_time?,
        boot_id: key.boot_id.unwrap_or_default(),
    })
}

fn register_from_proto(req: RegisterProcess) -> Option<RegisterRequest> {
    let mut sha = [0u8; 32];
    if req.exe_sha256.len() == 32 {
        sha.copy_from_slice(&req.exe_sha256);
    }
    let mut nonce = [0u8; 32];
    if req.registration_nonce.len() == 32 {
        nonce.copy_from_slice(&req.registration_nonce);
    }

    Some(RegisterRequest {
        key: key_from_proto(req.key?)?,
        exe_path: PathBuf::from(req.exe_path),
        exe_sha256: sha,
        app_class: req.app_class,
        app_name: req.app_name,
        app_version: req.app_version,
        instance_name: req.instance_name,
        allow_policy: AllowPolicy {
            allow_all_ops: req
                .allow_policy
                .as_ref()
                .map(|p| p.allow_all_ops)
                .unwrap_or(true),
            env_allowlist: req
                .allow_policy
                .map(|p| p.env_allowlist)
                .unwrap_or_default(),
        },
        disclosure: Disclosure {
            expose_exe_path: req
                .disclosure
                .as_ref()
                .map(|d| d.expose_exe_path)
                .unwrap_or(false),
            expose_cmdline: req
                .disclosure
                .as_ref()
                .map(|d| d.expose_cmdline)
                .unwrap_or(false),
            expose_env_names: req.disclosure.map(|d| d.expose_env_names).unwrap_or(false),
        },
        disclosed_cwd: (!req.disclosed_cwd.is_empty()).then(|| PathBuf::from(req.disclosed_cwd)),
        disclosed_env: req.disclosed_env.into_iter().collect(),
        nonce,
        supported_ops: req
            .supported_ops
            .into_iter()
            .filter_map(|op| match op {
                1 => Some("stack_capture".to_string()),
                2 => Some("cpu_profile".to_string()),
                3 => Some("heap_profile".to_string()),
                4 => Some("off_cpu_profile".to_string()),
                _ => None,
            })
            .collect(),
        runtime: Runtime::from_proto(req.runtime),
        symbol_source: req.symbol_source,
        symbol_manifest_path: (!req.symbol_manifest_path.is_empty())
            .then(|| PathBuf::from(req.symbol_manifest_path)),
        symbol_paths: req.symbol_paths.into_iter().map(PathBuf::from).collect(),
    })
}

/// Translate a wire crash filter.
///
/// Proto3 cannot distinguish an unset string from an empty one, so an empty
/// filter field means "not filtering" rather than "match the empty string" —
/// no crash row has an empty class or signature worth selecting for, and a
/// caller that sent nothing plainly meant nothing.
fn crash_filter_from_proto(query: wire::CrashQuery) -> CrashFilter {
    CrashFilter {
        app_class: non_empty(query.app_class),
        app_class_like: non_empty(query.app_class_like),
        app_name: non_empty(query.app_name),
        instance_name: non_empty(query.instance_name),
        signature: non_empty(query.signature),
        // Zero is "unbounded on this side", which is why the window is
        // half-open: a zero `until` would otherwise select nothing at all.
        since_unix_ms: (query.since_unix_ms != 0).then_some(query.since_unix_ms),
        until_unix_ms: (query.until_unix_ms != 0).then_some(query.until_unix_ms),
    }
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// Encode one crash record for the wire.
///
/// The daemon-private `artifact_path` is deliberately not carried: it
/// discloses the owner's directory layout on a surface whose whole contract is
/// redacted metadata. Callers get the opaque `id` and fetch bytes through the
/// artifact endpoint, which resolves the id itself.
fn crash_record_to_proto(record: &crate::crash_store::CrashRecord) -> wire::CrashRecord {
    wire::CrashRecord {
        key: Some(wire::ProcessKey {
            pid: u64::from(record.pid),
            start_time: Some(record.creation_time_ms),
            boot_id: None,
        }),
        signature: record.signature.clone(),
        crash_unix_ms: record.crashed_at_ms,
        app_class: record.app_class.clone(),
        app_name: record.app_name.clone(),
        app_version: record.app_version.clone(),
        instance_name: record.instance_name.clone(),
        fault_kind: record.fault_kind.clone(),
        arch: std::env::consts::ARCH.to_string(),
        os: std::env::consts::OS.to_string(),
        id: record.id,
        artifact_bytes: record.artifact_bytes,
        cwd: record.cwd.clone(),
    }
}

/// Encode a crash rollup for the wire.
fn crash_stats_to_proto(stats: &crate::crash_query::CrashStats) -> CrashStatsReply {
    CrashStatsReply {
        signatures: stats
            .signatures
            .iter()
            .map(|stat| wire::CrashSignatureStat {
                signature: stat.signature.clone(),
                count: stat.count,
                first_unix_ms: stat.first_unix_ms,
                last_unix_ms: stat.last_unix_ms,
                app_classes: stat.app_classes.clone(),
            })
            .collect(),
        total: stats.total,
        first_unix_ms: stats.first_unix_ms,
        last_unix_ms: stats.last_unix_ms,
        distinct_classes: stats.distinct_classes,
        error: 0,
        detail: String::new(),
    }
}

/// Encode a reply as a `ProbeEnvelope` for the wire.
pub fn envelope_from_reply(request_id: u64, reply: &ProbeReply) -> ProbeEnvelope {
    let body = match reply {
        ProbeReply::Armed { .. } => Body::RegistrationStatus(RegistrationStatus {
            // 2 == ARMED.
            state: 2,
            error: 0,
            detail: String::new(),
            ..Default::default()
        }),
        ProbeReply::Ack => Body::RegistrationStatus(RegistrationStatus {
            state: 0,
            error: 0,
            detail: "ack".into(),
            ..Default::default()
        }),
        ProbeReply::CaptureRequested(request) => Body::CaptureStack(request.clone()),
        ProbeReply::CaptureAccepted(reply) => Body::CaptureReply(reply.clone()),
        ProbeReply::JobStatus(status) => Body::JobStatus(status.clone()),
        ProbeReply::Crashes(records) => Body::CrashQueryReply(CrashQueryReply {
            records: records.iter().map(crash_record_to_proto).collect(),
            error: 0,
            detail: String::new(),
        }),
        ProbeReply::CrashStatistics(stats) => Body::CrashStatsReply(crash_stats_to_proto(stats)),
        // Refusals ride the reply body their request asked for, so a caller
        // matching on `CrashQueryReply` reads the error out of the field the
        // schema put there for it.
        ProbeReply::CrashRefused {
            code,
            reason,
            stats,
        } => {
            if *stats {
                Body::CrashStatsReply(CrashStatsReply {
                    error: probe_error_to_proto(*code),
                    detail: reason.clone(),
                    ..Default::default()
                })
            } else {
                Body::CrashQueryReply(CrashQueryReply {
                    records: Vec::new(),
                    error: probe_error_to_proto(*code),
                    detail: reason.clone(),
                })
            }
        }
        ProbeReply::Processes(processes) => Body::ProcessQueryReply(ProcessQueryReply {
            processes: processes.clone(),
            error: 0,
            detail: String::new(),
        }),
        ProbeReply::Refused { code, reason } => Body::RegistrationStatus(RegistrationStatus {
            // 3 == DROPPED: the request did not produce a live registration.
            state: 3,
            error: probe_error_to_proto(*code),
            detail: reason.clone(),
            ..Default::default()
        }),
    };
    ProbeEnvelope {
        wire_version: 1,
        request_id,
        deadline_unix_ms: 0,
        body: Some(body),
    }
}

/// Map the internal taxonomy onto `probe_diag.v1`'s `ProbeErrorCode`.
fn probe_error_to_proto(code: ProbeErrorCode) -> i32 {
    match code {
        ProbeErrorCode::MalformedRequest => 5, // PROBE_ERROR_INTERNAL
        ProbeErrorCode::OversizeField => 5,
        ProbeErrorCode::NonceReplay => 3, // POLICY_DENIED
        ProbeErrorCode::PeerRejected => 3,
        ProbeErrorCode::PolicyDenied => 3,
        ProbeErrorCode::NotArmed => 2, // NOT_REGISTERED
        ProbeErrorCode::NotRegistered => 2,
        ProbeErrorCode::IdentityMismatch => 1, // PID_REUSE / identity
    }
}
