//! `ProbeOps` — the transport-agnostic request core.
//!
//! [`ProbeOps::dispatch`] is a pure function of `(request, peer, conn_id,
//! registry state)`. It performs no I/O.
//!
//! That matters because the daemon has two ingresses — the framed control
//! socket and (later) an HTTP surface. If each owned its own policy logic they
//! would drift, and the weaker one would become the way in. Both call this,
//! so both enforce the same rules and return the same structured errors.
//!
//! It also makes the contract testable without sockets: every state-machine,
//! bounds, replay, and peer-rejection case below is driven by calling
//! `dispatch` directly.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use running_process::broker::server::{PeerCredentialPolicy, PeerIdentity};
use running_process_probe::probe_diag::v1::{
    CaptureReply, CaptureStackRequest, JobStatus, ProcessInfo,
};

use crate::capture_jobs::CaptureJobs;
use crate::crash_query::{CrashFilter, CrashStats};
use crate::crash_store::{CrashRecord, CrashStore, CrashStoreError};
use crate::query::{ProcessQuery, QueryEngine};
use crate::registry::{ProcessKey, RegisterError, RegisterRequest, Registry};
use crate::state::{RegState, StateError};

/// How often a registrant is expected to heartbeat.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// How long a registration survives without a heartbeat.
///
/// Three missed intervals. This only backstops SIGKILL and similar exits where
/// no connection close ever arrives — a clean disconnect drops the entry
/// immediately and never waits for this.
pub const HEARTBEAT_GRACE: Duration = Duration::from_secs(15);

/// Stable error taxonomy returned by both ingresses.
///
/// Callers branch on these, so the discriminants are part of the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeErrorCode {
    /// Request could not be decoded or violated its schema.
    MalformedRequest,
    /// A field exceeded its declared bound.
    OversizeField,
    /// Registration nonce was reused, or the nonce table is full.
    NonceReplay,
    /// Connecting peer is not the daemon's owner.
    PeerRejected,
    /// Registrant policy or advertised capabilities refused the operation.
    PolicyDenied,
    /// Operation requires an ARMED registration.
    NotArmed,
    /// Identity verification failed or was not performed.
    IdentityMismatch,
    /// No registration exists for the given key.
    NotRegistered,
}

/// A decoded request, independent of how it arrived.
#[derive(Clone, Debug)]
pub enum ProbeRequest {
    /// Enroll a process.
    Register(Box<RegisterRequest>),
    /// Refresh liveness.
    Heartbeat(ProcessKey),
    /// Voluntarily deregister. Best-effort only — liveness is the real
    /// mechanism, since a crashed process never sends this.
    Unregister(ProcessKey),
    /// Ask for a stack capture of the named process.
    CaptureStack {
        /// Target process.
        key: ProcessKey,
        /// Maximum native frames per thread.
        max_depth: u32,
        /// Optional thread-selection bitmask.
        thread_filter: u32,
        /// Absolute wire deadline, or zero for the daemon default.
        deadline_unix_ms: u64,
    },
    /// Target-produced raw artifact.
    CaptureResult(CaptureReply),
    /// Query one asynchronous capture.
    GetJobStatus(String),
    /// Query live registrations and, when requested, the OS process table.
    Query(Box<ProcessQuery>),
    /// Page through durable crash history.
    QueryCrashes {
        /// Which crashes to return.
        filter: Box<CrashFilter>,
        /// Maximum records. Mandatory, like the live query's.
        limit: NonZeroU32,
    },
    /// Roll up durable crash history by signature.
    CrashStats(Box<CrashFilter>),
}

/// The reply to a [`ProbeRequest`].
#[derive(Clone, Debug, PartialEq)]
pub enum ProbeReply {
    /// Registration reached ARMED.
    Armed {
        /// The armed identity.
        key: ProcessKey,
    },
    /// Request accepted, nothing further to report.
    Ack,
    /// A target should perform this capture now.
    CaptureRequested(CaptureStackRequest),
    /// An operator's capture was queued.
    CaptureAccepted(CaptureReply),
    /// Current asynchronous job state.
    JobStatus(JobStatus),
    /// Matching live processes.
    Processes(Vec<ProcessInfo>),
    /// Matching crash records, newest first and already limit-truncated.
    Crashes(Vec<CrashRecord>),
    /// Crash rollups over the whole match set.
    CrashStatistics(Box<CrashStats>),
    /// A crash query was refused.
    ///
    /// Distinct from [`ProbeReply::Refused`] so the refusal travels on the
    /// crash reply body. A caller that matched on `CrashQueryReply` would
    /// otherwise see an unrelated `RegistrationStatus` and have to treat
    /// "your limit was too large" as "unexpected message".
    CrashRefused {
        /// Machine-branchable classification.
        code: ProbeErrorCode,
        /// Human-readable detail.
        reason: String,
        /// Whether the refused request was a rollup rather than a page.
        stats: bool,
    },
    /// Request refused, with a stable code and a human-readable reason.
    Refused {
        /// Machine-branchable classification.
        code: ProbeErrorCode,
        /// Human-readable detail. Never parsed by callers.
        reason: String,
    },
}

/// Outcome of verifying a registrant's claimed identity.
///
/// Supplied by the caller rather than computed here: verification touches the
/// filesystem and the process table, and `dispatch` stays I/O-free.
#[derive(Clone, Copy, Debug)]
pub struct IdentityVerdict {
    /// Whether the claimed executable hash, boot id, and liveness all matched.
    pub verified: bool,
    /// Whether the registrant's connection is still open.
    pub connection_alive: bool,
}

/// The daemon's request core.
#[derive(Debug)]
pub struct ProbeOps {
    registry: Arc<Registry>,
    owner_policy: PeerCredentialPolicy,
    capture_jobs: CaptureJobs,
    query_engine: QueryEngine,
    /// Durable crash history, when this daemon opened one.
    ///
    /// Optional because the store is a filesystem resource that can fail to
    /// open (a full or read-only home directory), and the live surfaces —
    /// registration, capture, `ps` — must keep working when it does. Absent
    /// means crash queries are refused with a reason, not that the daemon
    /// declines to start.
    crash_store: Option<Arc<CrashStore>>,
}

impl ProbeOps {
    /// Build a core over `registry`, accepting only peers `owner_policy` allows.
    pub fn new(registry: Arc<Registry>, owner_policy: PeerCredentialPolicy) -> Self {
        Self {
            registry,
            owner_policy,
            capture_jobs: CaptureJobs::default(),
            query_engine: QueryEngine::default(),
            crash_store: None,
        }
    }

    /// Attach durable crash history to this core.
    pub fn with_crash_store(mut self, crash_store: Arc<CrashStore>) -> Self {
        self.crash_store = Some(crash_store);
        self
    }

    /// Borrow the crash store, if one is attached.
    pub fn crash_store(&self) -> Option<&Arc<CrashStore>> {
        self.crash_store.as_ref()
    }

    /// The uid/SID this daemon serves.
    ///
    /// Needed by the HTTP ingress, which has no peer credentials to read: a
    /// TCP connection carries no identity. The bearer token already answered
    /// "is this the owner" — it could only have come from an owner-only
    /// discovery file — so HTTP presents that owner and every policy below
    /// (ARMED state, env allowlists, disclosure flags) still applies
    /// unchanged.
    pub fn owner(&self) -> String {
        match &self.owner_policy {
            PeerCredentialPolicy::OwnerOnly { uid_or_sid } => uid_or_sid.clone(),
            #[allow(unreachable_patterns)]
            _ => String::new(),
        }
    }

    /// The owner policy, for tests that must confirm it agrees with the
    /// registry's owner string.
    #[doc(hidden)]
    pub fn owner_policy_for_test(&self) -> PeerCredentialPolicy {
        self.owner_policy.clone()
    }

    /// Borrow the registry (queries, reaping).
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Cooperative capture queue shared by all connections.
    pub fn capture_jobs(&self) -> &CaptureJobs {
        &self.capture_jobs
    }

    /// Handle one request.
    ///
    /// `verdict` carries the caller's identity-verification result and is used
    /// only for `Register`.
    pub fn dispatch(
        &self,
        req: ProbeRequest,
        peer: &PeerIdentity,
        conn_id: u64,
        verdict: IdentityVerdict,
    ) -> ProbeReply {
        // Checked for every request shape, before anything else. The listener
        // already applies this policy, but a second ingress might not, and
        // this is the single place both share.
        if !self.owner_policy.allows(peer) {
            return ProbeReply::Refused {
                code: ProbeErrorCode::PeerRejected,
                reason: format!("peer {} is not the daemon owner", peer.uid_or_sid),
            };
        }

        match req {
            ProbeRequest::Register(request) => self.register(*request, peer, conn_id, verdict),
            ProbeRequest::Heartbeat(key) => match self.registry.heartbeat(&key, conn_id) {
                Ok(()) => {
                    self.capture_jobs
                        .lease(&key, conn_id)
                        .map_or(ProbeReply::Ack, |mut request| {
                            if let Some(entry) = self.registry.get(&key) {
                                request.symbol_manifest_path = entry
                                    .symbol_manifest_path
                                    .map(|path| path.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                request.symbol_paths = entry
                                    .symbol_paths
                                    .iter()
                                    .map(|path| path.to_string_lossy().into_owned())
                                    .collect();
                            }
                            ProbeReply::CaptureRequested(request)
                        })
                }
                Err(e) => refuse(e),
            },
            ProbeRequest::CaptureStack {
                key,
                max_depth,
                thread_filter,
                deadline_unix_ms,
            } => self.capture_stack(key, max_depth, thread_filter, deadline_unix_ms),
            ProbeRequest::CaptureResult(_) => ProbeReply::Refused {
                code: ProbeErrorCode::MalformedRequest,
                reason: "capture results must arrive on the leased connection".into(),
            },
            ProbeRequest::GetJobStatus(job_id) => self.capture_jobs.status(&job_id).map_or_else(
                || ProbeReply::Refused {
                    code: ProbeErrorCode::NotRegistered,
                    reason: "no capture job with that id".into(),
                },
                ProbeReply::JobStatus,
            ),
            ProbeRequest::Query(query) => {
                ProbeReply::Processes(self.query_engine.run(&query, &self.registry))
            }
            ProbeRequest::QueryCrashes { filter, limit } => match self.crash_store.as_ref() {
                Some(store) => match store.query(&filter, limit) {
                    Ok(records) => ProbeReply::Crashes(records),
                    Err(error) => crash_refusal(&error, false),
                },
                None => no_crash_store(false),
            },
            ProbeRequest::CrashStats(filter) => match self.crash_store.as_ref() {
                Some(store) => match store.stats(&filter) {
                    Ok(stats) => ProbeReply::CrashStatistics(Box::new(stats)),
                    Err(error) => crash_refusal(&error, true),
                },
                None => no_crash_store(true),
            },
            ProbeRequest::Unregister(key) => {
                if let Some(entry) = self.registry.get(&key) {
                    if entry.conn_id == conn_id {
                        self.registry.drop_by_conn(conn_id);
                        ProbeReply::Ack
                    } else {
                        ProbeReply::Refused {
                            code: ProbeErrorCode::NotRegistered,
                            reason: "registration belongs to another connection".into(),
                        }
                    }
                } else {
                    ProbeReply::Refused {
                        code: ProbeErrorCode::NotRegistered,
                        reason: "no registration for this process key".into(),
                    }
                }
            }
        }
    }

    /// Validate a capture request against the registry.
    ///
    /// Answers the two questions that can actually differ here with separate
    /// refusals: an unknown process means the caller has the wrong key, an
    /// unarmed one means wait or re-register. Collapsing them would leave an
    /// operator guessing.
    ///
    /// No per-request ownership check: `dispatch` already refuses any peer the
    /// owner policy rejects, and the daemon is owner-only, so a registration
    /// belonging to a different user is unreachable from here. A check for it
    /// would be a branch nothing can take — a false promise about what this
    /// function guards.
    fn capture_stack(
        &self,
        key: ProcessKey,
        max_depth: u32,
        thread_filter: u32,
        deadline_unix_ms: u64,
    ) -> ProbeReply {
        let Some(entry) = self.registry.get(&key) else {
            return ProbeReply::Refused {
                code: ProbeErrorCode::NotRegistered,
                reason: "no registration for this process key".into(),
            };
        };

        if entry.state != RegState::Armed {
            return ProbeReply::Refused {
                code: ProbeErrorCode::NotArmed,
                reason: format!("process is {:?}, not ARMED", entry.state),
            };
        }

        if !entry.allow_policy.allow_all_ops
            || !entry.supported_ops.iter().any(|op| op == "stack_capture")
        {
            return ProbeReply::Refused {
                code: ProbeErrorCode::PolicyDenied,
                reason: "target did not permit and advertise stack capture".into(),
            };
        }

        // The operator receives an asynchronous receipt. The target leases
        // this request on its next heartbeat over its authenticated connection.
        match self
            .capture_jobs
            .enqueue(key, max_depth, thread_filter, deadline_unix_ms)
        {
            Ok(receipt) => ProbeReply::CaptureAccepted(receipt),
            Err(reason) => ProbeReply::Refused {
                code: ProbeErrorCode::MalformedRequest,
                reason: reason.into(),
            },
        }
    }

    fn register(
        &self,
        request: RegisterRequest,
        peer: &PeerIdentity,
        conn_id: u64,
        verdict: IdentityVerdict,
    ) -> ProbeReply {
        let key = match self.registry.begin_register(request, peer.clone(), conn_id) {
            Ok(k) => k,
            Err(e) => return refuse(e),
        };

        match self
            .registry
            .verify_and_arm(&key, verdict.verified, verdict.connection_alive)
        {
            Ok(()) => ProbeReply::Armed { key },
            Err(e) => refuse(e),
        }
    }
}

/// Map an internal error onto the stable wire taxonomy.
/// Refuse a crash query when this daemon has no durable store.
fn no_crash_store(stats: bool) -> ProbeReply {
    ProbeReply::CrashRefused {
        code: ProbeErrorCode::NotRegistered,
        reason: "this daemon has no crash history store".into(),
        stats,
    }
}

/// Classify a crash-store failure.
///
/// A refused *query* (bad limit, inverted window, oversize filter) is the
/// caller's mistake and says so; anything else is the daemon's problem and is
/// reported as internal rather than blamed on the request.
fn crash_refusal(error: &CrashStoreError, stats: bool) -> ProbeReply {
    let code = match error {
        CrashStoreError::Query(_) => ProbeErrorCode::MalformedRequest,
        _ => ProbeErrorCode::PolicyDenied,
    };
    ProbeReply::CrashRefused {
        code,
        reason: error.to_string(),
        stats,
    }
}

fn refuse(err: RegisterError) -> ProbeReply {
    let code = match &err {
        RegisterError::OversizeField { .. } => ProbeErrorCode::OversizeField,
        RegisterError::InvalidSymbolSource { .. } => ProbeErrorCode::MalformedRequest,
        RegisterError::UndeclaredEnvironment { .. } => ProbeErrorCode::PolicyDenied,
        RegisterError::NonceReplay => ProbeErrorCode::NonceReplay,
        RegisterError::PeerRejected { .. } => ProbeErrorCode::PeerRejected,
        RegisterError::NotRegistered => ProbeErrorCode::NotRegistered,
        RegisterError::State(StateError::IdentityUnverified) => ProbeErrorCode::IdentityMismatch,
        RegisterError::State(StateError::NoLiveProbe) => ProbeErrorCode::NotArmed,
        RegisterError::State(StateError::Illegal { .. }) => ProbeErrorCode::MalformedRequest,
    };
    ProbeReply::Refused {
        code,
        reason: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{AllowPolicy, Disclosure};
    use std::path::PathBuf;

    const OWNER: &str = "owner-uid";

    fn ops() -> ProbeOps {
        ProbeOps::new(
            Arc::new(Registry::new(OWNER.into())),
            PeerCredentialPolicy::OwnerOnly {
                uid_or_sid: OWNER.into(),
            },
        )
    }

    fn peer() -> PeerIdentity {
        PeerIdentity {
            pid: 1234,
            uid_or_sid: OWNER.into(),
        }
    }

    fn request(pid: u32, nonce: u8) -> Box<RegisterRequest> {
        Box::new(RegisterRequest {
            key: ProcessKey {
                pid,
                started_at_unix_ms: 100,
                boot_id: "boot-1".into(),
            },
            exe_path: PathBuf::from("/usr/bin/app"),
            exe_sha256: [0u8; 32],
            app_class: "clud".into(),
            app_name: "clud".into(),
            app_version: "1.0".into(),
            instance_name: String::new(),
            allow_policy: AllowPolicy {
                allow_all_ops: true,
                ..Default::default()
            },
            disclosure: Disclosure::default(),
            disclosed_cwd: None,
            disclosed_env: Default::default(),
            nonce: [nonce; 32],
            supported_ops: vec!["stack_capture".into()],
            runtime: crate::registry::Runtime::Native,
            symbol_source: 2,
            symbol_manifest_path: None,
            symbol_paths: Vec::new(),
        })
    }

    fn good() -> IdentityVerdict {
        IdentityVerdict {
            verified: true,
            connection_alive: true,
        }
    }

    fn capture_request(key: ProcessKey) -> ProbeRequest {
        ProbeRequest::CaptureStack {
            key,
            max_depth: 64,
            thread_filter: 0,
            deadline_unix_ms: 0,
        }
    }

    /// An unknown process must be refused as unregistered, not as malformed.
    #[test]
    fn a_capture_for_an_unknown_process_is_not_registered() {
        let ops = ops();
        let key = ProcessKey {
            pid: 999_999,
            started_at_unix_ms: 1,
            boot_id: "b".into(),
        };
        match ops.dispatch(capture_request(key), &peer(), 1, good()) {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::NotRegistered),
            other => panic!("expected NotRegistered, got {other:?}"),
        }
    }

    /// A registered-but-unarmed process is a different problem from an
    /// unknown one, and gets a different code so the caller can tell.
    #[test]
    fn a_capture_for_an_unarmed_process_says_so() {
        let ops = ops();
        let request = request(std::process::id(), 0x71);
        let key = request.key.clone();
        // Register without arming: identity not verified.
        let verdict = IdentityVerdict {
            verified: false,
            connection_alive: true,
        };
        let _ = ops.dispatch(ProbeRequest::Register(request), &peer(), 1, verdict);

        match ops.dispatch(capture_request(key), &peer(), 1, good()) {
            ProbeReply::Refused { code, reason } => {
                assert_eq!(code, ProbeErrorCode::NotArmed);
                assert!(
                    reason.contains("ARMED"),
                    "reason should name the state: {reason}"
                );
            }
            other => panic!("expected NotArmed, got {other:?}"),
        }
    }

    /// A foreign peer is stopped by the owner policy in `dispatch`, before
    /// the registry is consulted at all — so the refusal cannot disclose
    /// whether the process exists or what state it is in.
    ///
    /// An earlier version of this test claimed to exercise a per-request
    /// ownership check inside `capture_stack`. It did not: the policy refuses
    /// first, and with the daemon owner-only that check could never fire.
    /// Removing the check left this property, which is the one that matters.
    #[test]
    fn a_foreign_peer_is_refused_before_the_registry_is_consulted() {
        let ops = ops();
        let request = request(std::process::id(), 0x72);
        let key = request.key.clone();
        let verdict = IdentityVerdict {
            verified: false,
            connection_alive: true,
        };
        let _ = ops.dispatch(ProbeRequest::Register(request), &peer(), 1, verdict);

        let stranger = PeerIdentity {
            pid: 1234,
            uid_or_sid: "someone-else".into(),
        };
        match ops.dispatch(capture_request(key), &stranger, 2, good()) {
            ProbeReply::Refused { code, reason } => {
                assert_eq!(code, ProbeErrorCode::PeerRejected);
                assert!(
                    !reason.contains("ARMED") && !reason.contains("registration"),
                    "the refusal must not disclose registry state: {reason}"
                );
            }
            other => panic!("expected PeerRejected, got {other:?}"),
        }
    }

    #[test]
    fn register_with_verified_identity_arms() {
        let ops = ops();
        let reply = ops.dispatch(ProbeRequest::Register(request(10, 1)), &peer(), 1, good());
        assert!(matches!(reply, ProbeReply::Armed { .. }), "{reply:?}");
    }

    #[test]
    fn register_without_live_probe_is_refused_not_armed() {
        let ops = ops();
        let verdict = IdentityVerdict {
            verified: true,
            connection_alive: false,
        };
        let reply = ops.dispatch(ProbeRequest::Register(request(11, 2)), &peer(), 1, verdict);
        assert_eq!(
            reply,
            ProbeReply::Refused {
                code: ProbeErrorCode::NotArmed,
                reason: "cannot arm: no live probe connection".into(),
            }
        );
    }

    #[test]
    fn register_without_verified_identity_reports_identity_mismatch() {
        let ops = ops();
        let verdict = IdentityVerdict {
            verified: false,
            connection_alive: true,
        };
        let reply = ops.dispatch(ProbeRequest::Register(request(12, 3)), &peer(), 1, verdict);
        match reply {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::IdentityMismatch),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn foreign_peer_is_refused_before_any_work() {
        let ops = ops();
        let stranger = PeerIdentity {
            pid: 9,
            uid_or_sid: "someone-else".into(),
        };
        let reply = ops.dispatch(ProbeRequest::Register(request(13, 4)), &stranger, 1, good());
        match reply {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::PeerRejected),
            other => panic!("expected refusal, got {other:?}"),
        }
        assert!(ops.registry().is_empty());
    }

    #[test]
    fn heartbeat_from_a_foreign_peer_is_refused() {
        let ops = ops();
        let key = match ops.dispatch(ProbeRequest::Register(request(14, 5)), &peer(), 1, good()) {
            ProbeReply::Armed { key } => key,
            other => panic!("expected Armed, got {other:?}"),
        };
        let stranger = PeerIdentity {
            pid: 9,
            uid_or_sid: "someone-else".into(),
        };
        match ops.dispatch(ProbeRequest::Heartbeat(key), &stranger, 1, good()) {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::PeerRejected),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn replayed_nonce_is_refused_with_its_own_code() {
        let ops = ops();
        ops.dispatch(ProbeRequest::Register(request(15, 6)), &peer(), 1, good());
        let reply = ops.dispatch(ProbeRequest::Register(request(16, 6)), &peer(), 2, good());
        match reply {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::NonceReplay),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn oversize_field_is_refused_with_its_own_code() {
        let ops = ops();
        let mut req = request(17, 7);
        req.app_name = "x".repeat(1024);
        match ops.dispatch(ProbeRequest::Register(req), &peer(), 1, good()) {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::OversizeField),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn heartbeat_after_unregister_is_refused() {
        let ops = ops();
        let key = match ops.dispatch(ProbeRequest::Register(request(18, 8)), &peer(), 1, good()) {
            ProbeReply::Armed { key } => key,
            other => panic!("expected Armed, got {other:?}"),
        };
        assert_eq!(
            ops.dispatch(ProbeRequest::Unregister(key.clone()), &peer(), 1, good()),
            ProbeReply::Ack
        );
        match ops.dispatch(ProbeRequest::Heartbeat(key), &peer(), 1, good()) {
            ProbeReply::Refused { code, .. } => assert_eq!(code, ProbeErrorCode::NotRegistered),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn heartbeat_on_a_live_registration_is_acked() {
        let ops = ops();
        let key = match ops.dispatch(ProbeRequest::Register(request(19, 9)), &peer(), 1, good()) {
            ProbeReply::Armed { key } => key,
            other => panic!("expected Armed, got {other:?}"),
        };
        assert_eq!(
            ops.dispatch(ProbeRequest::Heartbeat(key), &peer(), 1, good()),
            ProbeReply::Ack
        );
    }

    #[test]
    fn an_eligible_capture_is_queued_then_leased_on_the_target_heartbeat() {
        let ops = ops();
        let key = match ops.dispatch(ProbeRequest::Register(request(20, 10)), &peer(), 41, good()) {
            ProbeReply::Armed { key } => key,
            other => panic!("expected Armed, got {other:?}"),
        };

        let receipt = match ops.dispatch(capture_request(key.clone()), &peer(), 99, good()) {
            ProbeReply::CaptureAccepted(receipt) => receipt,
            other => panic!("expected queued capture, got {other:?}"),
        };
        assert!(!receipt.job_id.is_empty());

        match ops.dispatch(ProbeRequest::Heartbeat(key.clone()), &peer(), 41, good()) {
            ProbeReply::CaptureRequested(request) => {
                assert_eq!(request.max_depth, 64);
                assert_eq!(request.key.expect("key").pid, u64::from(key.pid));
            }
            other => panic!("expected leased capture, got {other:?}"),
        }
        assert_eq!(
            ops.dispatch(ProbeRequest::Heartbeat(key), &peer(), 41, good()),
            ProbeReply::Ack,
            "a second job cannot be leased until the first upload arrives"
        );
    }

    /// #638: symbol paths come from the daemon's stored ARMED registration,
    /// not from mutable target configuration at capture time.
    #[test]
    fn a_capture_lease_carries_the_authoritative_registration_symbol_sources() {
        let ops = ops();
        let mut registration = request(21, 11);
        registration.symbol_source = 3;
        registration.symbol_manifest_path =
            Some(PathBuf::from("/symbols/app.rpprobe-symbols.json"));
        registration.symbol_paths = vec![PathBuf::from("/symbols/private")];
        let key = match ops.dispatch(ProbeRequest::Register(registration), &peer(), 41, good()) {
            ProbeReply::Armed { key } => key,
            other => panic!("expected Armed, got {other:?}"),
        };
        assert!(matches!(
            ops.dispatch(capture_request(key.clone()), &peer(), 99, good()),
            ProbeReply::CaptureAccepted(_)
        ));

        let ProbeReply::CaptureRequested(lease) =
            ops.dispatch(ProbeRequest::Heartbeat(key), &peer(), 41, good())
        else {
            panic!("expected capture lease");
        };
        assert_eq!(
            lease.symbol_manifest_path,
            "/symbols/app.rpprobe-symbols.json"
        );
        assert_eq!(lease.symbol_paths, vec!["/symbols/private"]);
    }

    /// The grace window only exists for exits that send no close.
    #[test]
    fn grace_is_three_missed_intervals() {
        assert_eq!(HEARTBEAT_GRACE, HEARTBEAT_INTERVAL * 3);
    }
}
