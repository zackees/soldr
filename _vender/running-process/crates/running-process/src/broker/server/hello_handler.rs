//! Hello validation and in-memory negotiation.
//!
//! This module is intentionally synchronous and side-effect-free. The
//! Phase 4 accept loop will call into it after peer-credential checks,
//! rate limiting, and service-definition loading have produced the
//! registered backend table.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use prost::Message;

use crate::broker::capabilities::{handoff_transport_available, CAP_HANDLE_PASSING};
use crate::broker::lifecycle::names::{validate_service_name, validate_version, PipePathError};
use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, validate_frame_envelope, ErrorCode, Frame, FrameKind,
    FrameValidationError, Hello, HelloReply, Negotiated, Refused, ServiceDefinition,
    CONTROL_PAYLOAD_PROTOCOL, PROTOCOL_VERSION,
};
use crate::broker::server::handoff::{
    AcknowledgedHandoff, ExpiredHandoff, HandoffAckError, HandoffAckRegistry, HandoffToken,
    HandoffTokenStore, PendingHandoffBackend,
};
use crate::broker::server::version_allow_list::{check_version_allowed, VersionPolicyBlock};
use crate::broker::server::TraceContext;

const DEFAULT_KEEPALIVE_SECS: u64 = 30 * 60;
const DEFAULT_RATE_LIMIT_MAX_PER_WINDOW: u32 = 256;
const DEFAULT_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(1);

/// OS-verified peer identity for the process that sent a Hello.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    /// Peer process ID from platform IPC credentials.
    pub pid: u32,
    /// User identifier or SID captured by the accept loop.
    pub uid_or_sid: String,
}

/// Decoded Hello request plus the envelope metadata that carried it.
#[derive(Clone, Debug)]
pub struct HelloRequest {
    /// Frozen v1 envelope frame. Trace context and request ID live here.
    pub frame: Frame,
    /// Decoded control-plane Hello payload.
    pub hello: Hello,
    /// OS-verified peer identity.
    pub peer: PeerIdentity,
}

impl HelloRequest {
    /// Decode a v1 control-plane Hello from a validated frame.
    pub fn decode(frame: Frame, peer: PeerIdentity) -> Result<Self, Refused> {
        validate_frame_envelope(&frame, FrameKind::Request, CONTROL_PAYLOAD_PROTOCOL).map_err(
            |error| match error {
                FrameValidationError::EnvelopeVersion { .. } => refused(
                    ErrorCode::ErrorVersionUnsupported,
                    "frame envelope_version is not v1",
                    0,
                ),
                FrameValidationError::Kind { .. } => refused(
                    ErrorCode::ErrorPeerRejected,
                    "Hello frame kind must be REQUEST",
                    0,
                ),
                FrameValidationError::PayloadProtocol { .. } => refused(
                    ErrorCode::ErrorPeerRejected,
                    "Hello frame payload_protocol must be control-plane",
                    0,
                ),
                FrameValidationError::PayloadEncoding { .. } => refused(
                    ErrorCode::ErrorPeerRejected,
                    "Hello payload must not be compressed",
                    0,
                ),
            },
        )?;
        let hello = Hello::decode(frame.payload.as_slice())
            .map_err(|_| refused(ErrorCode::ErrorPeerRejected, "malformed Hello payload", 0))?;
        Ok(Self { frame, hello, peer })
    }

    /// Trace context available to backend lifecycle and diagnostics.
    pub fn trace_context(&self) -> TraceContext {
        TraceContext::from_frame(&self.frame)
    }
}

/// Backend metadata already verified by the backend registry.
#[derive(Clone, Debug)]
pub struct RegisteredBackend {
    /// Service definition selected for this backend.
    pub service_definition: ServiceDefinition,
    /// Version string returned in `Negotiated.daemon_version`.
    pub daemon_version: String,
    /// Direct backend pipe/socket path returned to the client.
    pub backend_pipe: String,
    /// Capability bitmap exposed to the client.
    pub server_capabilities: u64,
}

/// Deterministic Hello handler over an in-memory backend table.
#[derive(Debug)]
pub struct HelloHandler {
    backends: HashMap<String, RegisteredBackend>,
    next_connection_id: AtomicU64,
    rate_limiter: PeerRateLimiter,
    handoff_tokens: Mutex<HandoffTokenStore>,
    handoff_acks: Mutex<HandoffAckRegistry>,
}

impl HelloHandler {
    /// Create an empty handler.
    pub fn new() -> Self {
        Self {
            backends: HashMap::new(),
            next_connection_id: AtomicU64::new(1),
            rate_limiter: PeerRateLimiter::default(),
            handoff_tokens: Mutex::new(HandoffTokenStore::new()),
            handoff_acks: Mutex::new(HandoffAckRegistry::new()),
        }
    }

    /// Override the backend ACK deadline for pending handoffs.
    pub fn with_handoff_ack_deadline(self, ack_deadline: Duration) -> Self {
        *self.handoff_ack_registry() = HandoffAckRegistry::with_ack_deadline(ack_deadline);
        self
    }

    /// Lock the pending handoff token store owned by this handler.
    ///
    /// The backend-side acceptance path
    /// (`backend_lib::accept_handed_off`) consumes pending tokens from
    /// this store exactly once.
    pub fn handoff_token_store(&self) -> std::sync::MutexGuard<'_, HandoffTokenStore> {
        self.handoff_tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Lock the pending handoff ACK registry owned by this handler.
    ///
    /// Every token issued during Hello negotiation is registered here and
    /// must be acknowledged via [`HelloHandler::acknowledge_handoff`] before
    /// the ACK deadline, or it is abandoned by
    /// [`HelloHandler::expire_overdue_handoffs`].
    pub fn handoff_ack_registry(&self) -> std::sync::MutexGuard<'_, HandoffAckRegistry> {
        self.handoff_acks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record that the backend adopted a handed-off connection.
    ///
    /// Completes the pending handoff registered at Hello time and revokes the
    /// one-time token. Lock order: ACK registry, then token store.
    pub fn acknowledge_handoff(
        &self,
        token: &HandoffToken,
        now: Instant,
    ) -> Result<AcknowledgedHandoff, HandoffAckError> {
        let mut acks = self.handoff_ack_registry();
        let mut tokens = self.handoff_token_store();
        acks.acknowledge(&mut tokens, token, now)
    }

    /// Abandon every pending handoff whose backend ACK deadline has passed.
    ///
    /// Each returned expiry has had its token revoked; callers must use the
    /// `backend_pipe` reconnect fallback for the affected connections.
    pub fn expire_overdue_handoffs(&self, now: Instant) -> Vec<ExpiredHandoff> {
        let mut acks = self.handoff_ack_registry();
        let mut tokens = self.handoff_token_store();
        acks.expire_overdue(&mut tokens, now)
    }

    /// Override the per-peer Hello rate limit.
    pub fn with_rate_limit(mut self, max_per_window: u32, window: Duration) -> Self {
        self.rate_limiter = PeerRateLimiter::new(max_per_window, window);
        self
    }

    /// Register a backend by its service definition's service name.
    pub fn with_backend(mut self, backend: RegisteredBackend) -> Result<Self, HelloHandlerError> {
        validate_service_name_for_result(&backend.service_definition.service_name)?;
        if !backend.service_definition.min_version.is_empty() {
            validate_version_for_result(&backend.service_definition.min_version)?;
        }
        for version in &backend.service_definition.version_allow_list {
            validate_version_for_result(version)?;
        }
        self.backends
            .insert(backend.service_definition.service_name.clone(), backend);
        Ok(self)
    }

    /// Decode and handle a framed v1 Hello request.
    pub fn handle_frame(&self, frame: Frame, peer: PeerIdentity) -> HelloReply {
        match HelloRequest::decode(frame, peer) {
            Ok(request) => self.handle_request(&request),
            Err(refused) => refused_reply(refused),
        }
    }

    /// Validate a decoded Hello request and return a v1 HelloReply.
    pub fn handle_request(&self, request: &HelloRequest) -> HelloReply {
        let hello = &request.hello;
        if let Some(refused) = validate_hello_shape(hello, &request.peer) {
            return refused_reply(refused);
        }
        if let Some(retry_after) = self.rate_limiter.check(request.peer.pid) {
            return refused_reply(refused(
                ErrorCode::ErrorRateLimited,
                "Hello rate limit exceeded",
                duration_to_retry_ms(retry_after),
            ));
        }

        let Some(backend) = self.backends.get(&hello.service_name) else {
            return refused_reply(refused(
                ErrorCode::ErrorServiceUnknown,
                "service is not registered",
                0,
            ));
        };

        if let Some(refused) = validate_version_policy(hello, &backend.service_definition) {
            return refused_reply(refused);
        }

        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let handle_passed_token =
            self.issue_handoff_token(hello.client_capabilities, &hello.service_name);
        let mut server_capabilities = backend.server_capabilities;
        if !handle_passed_token.is_empty() {
            server_capabilities |= CAP_HANDLE_PASSING;
        }
        refused_or_negotiated(HelloReplyResult::Negotiated(Negotiated {
            negotiated_protocol: PROTOCOL_VERSION,
            daemon_version: backend.daemon_version.clone(),
            backend_pipe: backend.backend_pipe.clone(),
            warnings: Vec::new(),
            server_capabilities,
            keepalive_interval_secs: if hello.client_keepalive_secs == 0 {
                DEFAULT_KEEPALIVE_SECS
            } else {
                hello.client_keepalive_secs
            },
            handle_passed_token,
            connection_id,
        }))
    }

    /// Issue a pending handoff token when both sides support handle passing.
    ///
    /// Returns the 16 token bytes for `Negotiated.handle_passed_token`, or an
    /// empty vec when the client did not advertise [`CAP_HANDLE_PASSING`], the
    /// build lacks a handoff transport, or issuance failed (capacity or
    /// randomness). Issuance failure silently downgrades to the reconnect
    /// path: the reply omits both the token and the capability bit so the
    /// client never expects a handoff that cannot happen.
    ///
    /// Each issued token is also registered as awaiting a backend ACK; the
    /// handoff is only complete once [`HelloHandler::acknowledge_handoff`]
    /// succeeds before the registry deadline.
    fn issue_handoff_token(&self, client_capabilities: u64, service_name: &str) -> Vec<u8> {
        if client_capabilities & CAP_HANDLE_PASSING == 0 || !handoff_transport_available() {
            return Vec::new();
        }
        let now = Instant::now();
        // Lock order: ACK registry, then token store (matches the ACK paths).
        let mut acks = self.handoff_ack_registry();
        let mut tokens = self.handoff_token_store();
        match tokens.issue(now) {
            Ok(token) => {
                acks.register(token, PendingHandoffBackend::for_service(service_name), now);
                token.into_bytes().to_vec()
            }
            Err(_) => Vec::new(),
        }
    }
}

impl Default for HelloHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors raised while constructing a handler table.
#[derive(Debug, thiserror::Error)]
pub enum HelloHandlerError {
    /// A service definition field failed validation.
    #[error(transparent)]
    PipePath(#[from] PipePathError),
}

/// Per-peer PID token bucket for the Hello path.
#[derive(Debug)]
struct PeerRateLimiter {
    max_per_window: u32,
    window: Duration,
    entries: Mutex<HashMap<u32, PeerRateWindow>>,
}

impl PeerRateLimiter {
    fn new(max_per_window: u32, window: Duration) -> Self {
        Self {
            max_per_window: max_per_window.max(1),
            window: if window.is_zero() {
                Duration::from_millis(1)
            } else {
                window
            },
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn check(&self, pid: u32) -> Option<Duration> {
        if pid == 0 {
            return None;
        }

        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = entries.entry(pid).or_insert(PeerRateWindow {
            started_at: now,
            count: 0,
        });
        let elapsed = now.duration_since(entry.started_at);
        if elapsed >= self.window {
            entry.started_at = now;
            entry.count = 0;
        }

        if entry.count < self.max_per_window {
            entry.count += 1;
            None
        } else {
            Some(self.window.saturating_sub(elapsed))
        }
    }
}

impl Default for PeerRateLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_RATE_LIMIT_MAX_PER_WINDOW, DEFAULT_RATE_LIMIT_WINDOW)
    }
}

#[derive(Debug)]
struct PeerRateWindow {
    started_at: Instant,
    count: u32,
}

fn validate_hello_shape(hello: &Hello, peer: &PeerIdentity) -> Option<Refused> {
    if hello.client_min_protocol > PROTOCOL_VERSION || hello.client_max_protocol < PROTOCOL_VERSION
    {
        return Some(refused(
            ErrorCode::ErrorVersionUnsupported,
            "client protocol range does not include v1",
            0,
        ));
    }
    if validate_service_name(&hello.service_name).is_err() {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "invalid service_name",
            0,
        ));
    }
    if hello.wanted_version.len() > 64 || validate_version(&hello.wanted_version).is_err() {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "invalid wanted_version",
            0,
        ));
    }
    if hello.client_version.len() > 128 {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "client_version exceeds 128 bytes",
            0,
        ));
    }
    if hello.client_lib_name.len() > 64 || hello.client_lib_version.len() > 64 {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "client_lib fields exceed 64 bytes",
            0,
        ));
    }
    // peer.pid == 0 means the kernel did not report a peer pid (macOS
    // LOCAL_PEERCRED has no pid field), so there is nothing to cross-check.
    if hello.peer_pid != 0 && peer.pid != 0 && hello.peer_pid != peer.pid {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "peer_pid does not match verified peer",
            0,
        ));
    }
    None
}

fn validate_version_policy(hello: &Hello, service: &ServiceDefinition) -> Option<Refused> {
    match check_version_allowed(&hello.wanted_version, service) {
        Ok(()) => None,
        Err(VersionPolicyBlock::BelowMinVersion) => Some(refused(
            ErrorCode::ErrorVersionBlocked,
            "wanted_version is below min_version",
            30_000,
        )),
        Err(VersionPolicyBlock::OutsideAllowList) => Some(refused(
            ErrorCode::ErrorVersionBlocked,
            "wanted_version is not in version_allow_list",
            30_000,
        )),
    }
}

fn validate_service_name_for_result(name: &str) -> Result<(), HelloHandlerError> {
    validate_service_name(name).map_err(HelloHandlerError::PipePath)
}

fn validate_version_for_result(version: &str) -> Result<(), HelloHandlerError> {
    validate_version(version).map_err(HelloHandlerError::PipePath)
}

fn duration_to_retry_ms(duration: Duration) -> u64 {
    let millis = duration.as_millis().max(1);
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn refused(code: ErrorCode, reason: impl Into<String>, retry_after_ms: u64) -> Refused {
    Refused {
        reason: reason.into(),
        daemon_min_protocol: PROTOCOL_VERSION,
        daemon_max_protocol: PROTOCOL_VERSION,
        code: code as i32,
        details: HashMap::new(),
        retry_after_ms,
    }
}

fn refused_reply(refused: Refused) -> HelloReply {
    refused_or_negotiated(HelloReplyResult::Refused(refused))
}

fn refused_or_negotiated(result: HelloReplyResult) -> HelloReply {
    HelloReply {
        result: Some(result),
    }
}
