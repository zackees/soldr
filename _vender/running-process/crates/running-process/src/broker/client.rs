//! Client-side helpers for the v1 broker Hello path.

use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use interprocess::local_socket::prelude::*;
use prost::Message;

use crate::broker::capabilities::{handoff_transport_available, CAP_HANDLE_PASSING};
use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, read_frame, validate_frame_envelope, write_frame,
    AdminReply, AdminRequest, ErrorCode, Frame, FrameKind, FrameValidationError, FramingError,
    HandoffAck, Hello, HelloReply, Negotiated, PayloadEncoding, ADMIN_PAYLOAD_PROTOCOL,
    CONTROL_PAYLOAD_PROTOCOL, PROTOCOL_VERSION,
};
use crate::broker::server::handoff::validate_handoff_frame;
use crate::broker::server::local_socket_name;

/// Default wall-clock bound on waiting for the broker's handoff-ready relay
/// before silently downgrading to the `backend_pipe` reconnect path.
pub const DEFAULT_HANDOFF_READY_TIMEOUT: Duration = Duration::from_secs(2);

/// Canonical emergency escape hatch for participating broker consumers.
pub const RUNNING_PROCESS_DISABLE_ENV: &str = "RUNNING_PROCESS_DISABLE";
/// Value that disables broker usage and keeps the consumer on its direct path.
pub const RUNNING_PROCESS_DISABLE_VALUE: &str = "1";
/// TEST-ONLY seam that points the client at a fake backend endpoint (#354).
///
/// When set and non-empty, [`connect_to_backend`] connects directly to the
/// given endpoint (same local-socket transport as the Hello-skip cache path)
/// and skips broker discovery, Hello negotiation, and version checks
/// entirely. A connect failure is returned as-is — there is no fallback to
/// the real broker path, so tests that set this seam stay deterministic.
///
/// **Never set this in production.** It bypasses every broker safety check.
/// The canonical escape hatch [`RUNNING_PROCESS_DISABLE_ENV`]`=1` takes
/// precedence: when the broker is disabled, the fake-backend seam is ignored
/// too.
pub const RUNNING_PROCESS_FAKE_BACKEND_ENV: &str = "RUNNING_PROCESS_FAKE_BACKEND";

/// Return whether the canonical broker escape hatch is enabled.
///
/// This helper only parses the shared environment contract. Consumers still
/// own the direct fallback path they should use when this returns `true`.
pub fn broker_disabled_by_env() -> Result<bool, BrokerDisableEnvError> {
    let Some(value) = std::env::var_os(RUNNING_PROCESS_DISABLE_ENV) else {
        return Ok(false);
    };
    let value = value.to_string_lossy();
    if value == RUNNING_PROCESS_DISABLE_VALUE {
        Ok(true)
    } else {
        Err(BrokerDisableEnvError {
            value: value.into_owned(),
        })
    }
}

/// Inputs for [`connect_to_backend`].
#[derive(Clone, Debug)]
pub struct ConnectBackendRequest<'a> {
    /// Broker pipe/socket endpoint.
    pub broker_endpoint: &'a str,
    /// Logical service name, such as `zccache`.
    pub service_name: &'a str,
    /// Backend version the caller wants.
    pub wanted_version: &'a str,
    /// Version of the caller's own service binary.
    pub self_version: &'a str,
    /// Previously negotiated backend endpoint, if the caller has one.
    pub cached_backend_endpoint: Option<&'a str>,
    /// Informational client version.
    pub client_version: &'a str,
    /// Client library name for diagnostics.
    pub client_lib_name: &'a str,
    /// Client library version for diagnostics.
    pub client_lib_version: &'a str,
    /// Proposed keepalive interval.
    pub client_keepalive_secs: u64,
    /// Opt in to adopting a handed-off backend connection (#354, slice 7).
    ///
    /// Default `false`: the client always reconnects to
    /// `Negotiated.backend_pipe`, exactly as before. When `true` AND the
    /// broker negotiated [`CAP_HANDLE_PASSING`] AND issued a non-empty
    /// `Negotiated.handle_passed_token`, the client waits up to
    /// [`Self::handoff_ready_timeout`] for the broker's handoff-ready relay
    /// (an EVENT frame under the `0xD0FF` handoff payload protocol carrying
    /// the backend's `HandoffAck`) on the SAME broker connection. On a valid
    /// accepted relay with a matching token echo, the client keeps that
    /// connection as the backend connection
    /// ([`BackendConnectionRoute::HandlePassed`]). Any failure — missing
    /// relay, timeout, refused or malformed ACK, token mismatch — silently
    /// downgrades to the `backend_pipe` reconnect; adoption failure is never
    /// an error by itself.
    pub adopt_handed_off_connection: bool,
    /// Deadline for the handoff-ready relay when
    /// [`Self::adopt_handed_off_connection`] is set.
    pub handoff_ready_timeout: Duration,
}

impl<'a> ConnectBackendRequest<'a> {
    /// Build a request with running-process defaults.
    pub fn new(
        broker_endpoint: &'a str,
        service_name: &'a str,
        wanted_version: &'a str,
        self_version: &'a str,
    ) -> Self {
        Self {
            broker_endpoint,
            service_name,
            wanted_version,
            self_version,
            cached_backend_endpoint: None,
            client_version: "",
            client_lib_name: "running-process",
            client_lib_version: env!("CARGO_PKG_VERSION"),
            client_keepalive_secs: 0,
            adopt_handed_off_connection: false,
            handoff_ready_timeout: DEFAULT_HANDOFF_READY_TIMEOUT,
        }
    }

    fn can_hello_skip(&self) -> bool {
        self.cached_backend_endpoint.is_some() && self.wanted_version == self.self_version
    }

    fn hello(&self) -> Hello {
        Hello {
            client_min_protocol: PROTOCOL_VERSION,
            client_max_protocol: PROTOCOL_VERSION,
            service_name: self.service_name.into(),
            wanted_version: self.wanted_version.into(),
            client_version: self.client_version.into(),
            client_capabilities: client_capabilities(),
            auth_token: Vec::new(),
            request_id: "hello".into(),
            connection_id: 0,
            peer_pid: std::process::id(),
            client_lib_name: self.client_lib_name.into(),
            client_lib_version: self.client_lib_version.into(),
            peer_attestation_nonce: Vec::new(),
            capability_token: Vec::new(),
            client_keepalive_secs: self.client_keepalive_secs,
        }
    }
}

/// Capability bitmap this client advertises in `Hello.client_capabilities`.
///
/// [`CAP_HANDLE_PASSING`] is advertised only when the build carries a
/// platform handoff transport (Windows `DuplicateHandle`, Unix
/// `SCM_RIGHTS`) — currently both, but kept explicit so an exotic target
/// degrades cleanly to the reconnect path.
fn client_capabilities() -> u64 {
    if handoff_transport_available() {
        CAP_HANDLE_PASSING
    } else {
        0
    }
}

/// How [`connect_to_backend`] reached the returned backend endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendConnectionRoute {
    /// Connected directly to a known backend endpoint, skipping Hello.
    ///
    /// Used for the cached-endpoint fast path and, deliberately reused to
    /// avoid a new enum variant (a semver hazard for exhaustive matches),
    /// for the [`RUNNING_PROCESS_FAKE_BACKEND_ENV`] test seam. A
    /// fake-backend connection is distinguishable because the caller set the
    /// env var and [`BackendConnection::endpoint`] equals its value.
    HelloSkip,
    /// Asked the broker via Hello, then connected to the negotiated endpoint.
    BrokerNegotiated,
    /// Adopted the existing broker connection after a confirmed handoff.
    ///
    /// The broker handed the client's connection to the backend
    /// (`DuplicateHandle`/`SCM_RIGHTS`) and relayed the backend's accepted
    /// `HandoffAck` back to the client, so the socket that carried Hello is
    /// now served by the backend. No connection to `backend_pipe` was
    /// opened; [`BackendConnection::endpoint`] still reports the negotiated
    /// `backend_pipe` so callers can cache it for future Hello-skip.
    HandlePassed,
}

/// Open backend connection returned by [`connect_to_backend`].
#[derive(Debug)]
pub struct BackendConnection {
    /// Connected local socket stream.
    pub stream: interprocess::local_socket::Stream,
    /// Endpoint that was connected.
    ///
    /// For [`BackendConnectionRoute::HandlePassed`] this is the negotiated
    /// `backend_pipe` — useful as the Hello-skip cache key — even though the
    /// stream is the original broker connection rather than a fresh connect
    /// to that endpoint.
    pub endpoint: String,
    /// Route used to establish the connection.
    pub route: BackendConnectionRoute,
    /// Broker negotiation metadata when the broker path was used.
    pub negotiated: Option<Negotiated>,
}

impl BackendConnection {
    /// Pending one-time handoff token issued by the broker, if any.
    ///
    /// Non-empty only when both sides negotiated `CAP_HANDLE_PASSING`. By
    /// default the client still connects via `Negotiated.backend_pipe` and
    /// the route stays [`BackendConnectionRoute::BrokerNegotiated`]; when the
    /// caller opted in via
    /// [`ConnectBackendRequest::adopt_handed_off_connection`] and the broker
    /// confirmed the handoff, the route is
    /// [`BackendConnectionRoute::HandlePassed`] and this token is the one the
    /// confirmation echoed (#354).
    pub fn handoff_token(&self) -> Option<&[u8]> {
        self.negotiated
            .as_ref()
            .map(|negotiated| negotiated.handle_passed_token.as_slice())
            .filter(|token| !token.is_empty())
    }
}

/// Connect to a backend with the v1 Hello-skip fast path.
///
/// TEST seam: when [`RUNNING_PROCESS_FAKE_BACKEND_ENV`] is set to a
/// non-empty endpoint (and `RUNNING_PROCESS_DISABLE=1` is not engaged), the
/// client connects directly to that endpoint and returns
/// [`BackendConnectionRoute::HelloSkip`] with no negotiation. A connect
/// failure is returned ([`BrokerClientError::BackendConnect`]) without
/// falling back to the broker path. Never set the seam in production.
///
/// When `cached_backend_endpoint` is present and `wanted_version ==
/// self_version`, this tries the cached backend endpoint first. On miss,
/// or when the versions differ, it sends a broker `Hello`, reads the
/// broker `HelloReply`, and connects to `Negotiated.backend_pipe`.
///
/// With [`ConnectBackendRequest::adopt_handed_off_connection`] set and a
/// negotiated handoff (capability bit + non-empty token), the client first
/// waits — bounded by [`ConnectBackendRequest::handoff_ready_timeout`] — for
/// the broker's handoff-ready relay on the same connection and, when the
/// relay confirms the backend accepted, keeps that connection as the backend
/// connection. Any adoption failure silently falls back to the
/// `backend_pipe` reconnect below; reconnect remains the authoritative
/// correctness path.
pub fn connect_to_backend(
    request: ConnectBackendRequest<'_>,
) -> Result<BackendConnection, BrokerClientError> {
    #[cfg(feature = "test-seams")]
    if let Some(endpoint) = fake_backend_endpoint_from_env() {
        let stream = connect_local_socket(&endpoint).map_err(BrokerClientError::BackendConnect)?;
        return Ok(BackendConnection {
            stream,
            endpoint,
            route: BackendConnectionRoute::HelloSkip,
            negotiated: None,
        });
    }

    if request.can_hello_skip() {
        if let Some(endpoint) = request.cached_backend_endpoint {
            if let Ok(stream) = connect_local_socket(endpoint) {
                return Ok(BackendConnection {
                    stream,
                    endpoint: endpoint.into(),
                    route: BackendConnectionRoute::HelloSkip,
                    negotiated: None,
                });
            }
        }
    }

    let (broker_stream, negotiated) = broker_hello(&request)?;
    if request.adopt_handed_off_connection && handoff_negotiated(&negotiated) {
        if let Some(adopted) = await_handoff_ready(
            broker_stream,
            negotiated.handle_passed_token.clone(),
            request.handoff_ready_timeout,
        ) {
            return Ok(BackendConnection {
                endpoint: negotiated.backend_pipe.clone(),
                stream: adopted,
                route: BackendConnectionRoute::HandlePassed,
                negotiated: Some(negotiated),
            });
        }
    }

    if negotiated.backend_pipe.is_empty() {
        return Err(BrokerClientError::EmptyBackendPipe);
    }
    let stream = connect_local_socket(&negotiated.backend_pipe)
        .map_err(BrokerClientError::BackendConnect)?;
    Ok(BackendConnection {
        endpoint: negotiated.backend_pipe.clone(),
        stream,
        route: BackendConnectionRoute::BrokerNegotiated,
        negotiated: Some(negotiated),
    })
}

/// Read the [`RUNNING_PROCESS_FAKE_BACKEND_ENV`] test seam, if active.
///
/// Returns `Some(endpoint)` only when the variable is set to a non-empty
/// value AND the canonical disable hatch is not engaged
/// (`RUNNING_PROCESS_DISABLE=1` takes precedence — a disabled broker ignores
/// the fake seam too, mirroring the consumer-side disable contract). An
/// invalid `RUNNING_PROCESS_DISABLE` value is a configuration error that
/// [`broker_disabled_by_env`] surfaces to consumers before they reach
/// `connect_to_backend`; it does not suppress the seam here.
///
/// Gated behind the off-by-default `test-seams` feature (#433 R4) so the test
/// backdoor is physically absent from every production build of
/// [`connect_to_backend`]. Consumers depend on `running-process` with
/// `features = ["client", ...]`; `test-seams` is never in that set.
#[cfg(feature = "test-seams")]
fn fake_backend_endpoint_from_env() -> Option<String> {
    let value = std::env::var_os(RUNNING_PROCESS_FAKE_BACKEND_ENV)?;
    let value = value.to_string_lossy();
    if value.is_empty() {
        return None;
    }
    if matches!(broker_disabled_by_env(), Ok(true)) {
        return None;
    }
    Some(value.into_owned())
}

/// True when the broker negotiated handle passing for this connection: the
/// server capability bit is set AND a one-time token was issued.
fn handoff_negotiated(negotiated: &Negotiated) -> bool {
    negotiated.server_capabilities & CAP_HANDLE_PASSING == CAP_HANDLE_PASSING
        && !negotiated.handle_passed_token.is_empty()
}

/// Wait (bounded) for the broker's handoff-ready relay on the Hello
/// connection and return the stream when adoption is confirmed.
///
/// The relay is an EVENT frame under the handoff payload protocol
/// (`0xD0FF`) whose payload is the backend's `HandoffAck`; the client
/// requires the token echo to match its negotiated one-time token and
/// `accepted = true`. The blocking framed read runs on a helper thread so
/// the wait is strictly deadline-bounded even though local-socket streams
/// have no portable read timeout; on timeout the stream stays with the
/// helper thread (which exits as soon as the abandoned read resolves) and
/// the caller falls back to reconnect. Every failure returns `None` —
/// adoption is best-effort by contract.
fn await_handoff_ready(
    stream: interprocess::local_socket::Stream,
    expected_token: Vec<u8>,
    timeout: Duration,
) -> Option<interprocess::local_socket::Stream> {
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stream = stream;
        let outcome = read_handoff_ready(&mut stream, &expected_token).map(|()| stream);
        let _ = result_tx.send(outcome);
    });
    match result_rx.recv_timeout(timeout) {
        Ok(Ok(stream)) => Some(stream),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// Read and validate one handoff-ready relay frame.
///
/// Errors carry a static description for diagnostics, but the adoption
/// contract maps every failure to the silent reconnect downgrade.
fn read_handoff_ready(
    stream: &mut interprocess::local_socket::Stream,
    expected_token: &[u8],
) -> Result<(), &'static str> {
    let bytes = read_frame(stream).map_err(|_| "failed to read handoff-ready frame")?;
    let frame =
        Frame::decode(bytes.as_slice()).map_err(|_| "failed to decode handoff-ready Frame")?;
    validate_handoff_frame(&frame, FrameKind::Event)?;
    let ack = HandoffAck::decode(frame.payload.as_slice())
        .map_err(|_| "failed to decode handoff-ready HandoffAck payload")?;
    if ack.token != expected_token {
        return Err("handoff-ready token echo does not match the negotiated token");
    }
    if !ack.accepted {
        return Err("broker relayed a refused handoff");
    }
    Ok(())
}

/// Default deadline for a broker client round-trip (Hello / admin
/// request). Bounds the blocking connect + write + read so a broker that
/// accepts the connection then stalls before replying can't wedge the
/// caller forever (issue #590, cluster H). Override with
/// `RUNNING_PROCESS_BROKER_CLIENT_TIMEOUT_MS` (milliseconds).
const DEFAULT_BROKER_CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
const BROKER_CLIENT_TIMEOUT_ENV: &str = "RUNNING_PROCESS_BROKER_CLIENT_TIMEOUT_MS";

fn parse_broker_client_timeout(raw: Option<&str>) -> Duration {
    raw.and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_BROKER_CLIENT_TIMEOUT)
}

fn broker_client_deadline() -> Duration {
    parse_broker_client_timeout(std::env::var(BROKER_CLIENT_TIMEOUT_ENV).ok().as_deref())
}

fn broker_client_timeout_err() -> BrokerClientError {
    BrokerClientError::BrokerConnect(io::Error::new(
        io::ErrorKind::TimedOut,
        "broker client round-trip did not complete within the deadline",
    ))
}

/// Send one typed admin request to a broker endpoint and return its reply.
///
/// The blocking connect + write + read round-trip runs on a helper thread
/// bounded by `broker_client_deadline` (issue #590, cluster H); on
/// timeout the helper thread owns and drops the abandoned stream so a
/// stalled broker never wedges the caller.
pub fn send_admin_request(
    broker_endpoint: &str,
    request: AdminRequest,
) -> Result<AdminReply, BrokerClientError> {
    let endpoint = broker_endpoint.to_string();
    let (tx, rx) = mpsc::channel();
    // Free-function `thread::spawn` (a thread, not a process spawn), so the
    // spawn-path guard leaves it alone — same as `broker::client_v2`.
    thread::spawn(move || {
        let _ = tx.send(send_admin_request_unbounded(&endpoint, request));
    });
    match rx.recv_timeout(broker_client_deadline()) {
        Ok(result) => result,
        Err(_) => Err(broker_client_timeout_err()),
    }
}

fn send_admin_request_unbounded(
    broker_endpoint: &str,
    request: AdminRequest,
) -> Result<AdminReply, BrokerClientError> {
    let mut stream =
        connect_local_socket(broker_endpoint).map_err(BrokerClientError::BrokerConnect)?;
    let request_frame = Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: ADMIN_PAYLOAD_PROTOCOL,
        payload: request.encode_to_vec(),
        request_id: 1,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };
    write_frame(&mut stream, &request_frame.encode_to_vec())?;

    let response_bytes = read_frame(&mut stream)?;
    let response_frame =
        Frame::decode(response_bytes.as_slice()).map_err(BrokerClientError::DecodeFrame)?;
    validate_response_frame(
        &response_frame,
        ADMIN_PAYLOAD_PROTOCOL,
        "payload_protocol is not admin",
    )?;
    AdminReply::decode(response_frame.payload.as_slice())
        .map_err(BrokerClientError::DecodeAdminReply)
}

/// Open a platform local socket by broker endpoint string.
pub fn connect_local_socket(endpoint: &str) -> io::Result<interprocess::local_socket::Stream> {
    let name = local_socket_name(endpoint)?;
    LocalSocketStream::connect(name)
}

fn broker_hello(
    request: &ConnectBackendRequest<'_>,
) -> Result<(interprocess::local_socket::Stream, Negotiated), BrokerClientError> {
    // Bound the Hello handshake round-trip on a helper thread (issue #590,
    // cluster H). `request` is borrowed, so capture the owned endpoint +
    // pre-encoded Hello payload before moving into the thread; the
    // negotiated stream (Send) is handed back through the channel.
    let endpoint = request.broker_endpoint.to_string();
    let hello_bytes = request.hello().encode_to_vec();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(broker_hello_unbounded(&endpoint, hello_bytes));
    });
    match rx.recv_timeout(broker_client_deadline()) {
        Ok(result) => result,
        Err(_) => Err(broker_client_timeout_err()),
    }
}

fn broker_hello_unbounded(
    broker_endpoint: &str,
    hello_bytes: Vec<u8>,
) -> Result<(interprocess::local_socket::Stream, Negotiated), BrokerClientError> {
    let mut stream =
        connect_local_socket(broker_endpoint).map_err(BrokerClientError::BrokerConnect)?;
    let request_frame = Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: CONTROL_PAYLOAD_PROTOCOL,
        payload: hello_bytes,
        request_id: 1,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };
    write_frame(&mut stream, &request_frame.encode_to_vec())?;

    let response_bytes = read_frame(&mut stream)?;
    let response_frame =
        Frame::decode(response_bytes.as_slice()).map_err(BrokerClientError::DecodeFrame)?;
    validate_response_frame(
        &response_frame,
        CONTROL_PAYLOAD_PROTOCOL,
        "payload_protocol is not control-plane",
    )?;
    let reply = HelloReply::decode(response_frame.payload.as_slice())
        .map_err(BrokerClientError::DecodeHelloReply)?;
    match reply
        .result
        .ok_or(BrokerClientError::MissingHelloReplyResult)?
    {
        HelloReplyResult::Negotiated(negotiated) => Ok((stream, negotiated)),
        HelloReplyResult::Refused(refused) => Err(BrokerClientError::Refused {
            code: ErrorCode::try_from(refused.code).unwrap_or(ErrorCode::Unspecified),
            reason: refused.reason,
            retry_after_ms: refused.retry_after_ms,
        }),
    }
}

fn validate_response_frame(
    frame: &Frame,
    expected_payload_protocol: u32,
    payload_protocol_error: &'static str,
) -> Result<(), BrokerClientError> {
    validate_frame_envelope(frame, FrameKind::Response, expected_payload_protocol).map_err(
        |error| {
            BrokerClientError::UnexpectedResponseFrame(match error {
                FrameValidationError::EnvelopeVersion { .. } => "envelope_version is not v1",
                FrameValidationError::Kind { .. } => "kind is not RESPONSE",
                FrameValidationError::PayloadProtocol { .. } => payload_protocol_error,
                FrameValidationError::PayloadEncoding { .. } => "payload is compressed",
            })
        },
    )
}

/// Invalid value for the canonical broker disable variable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("RUNNING_PROCESS_DISABLE must be unset or 1, got {value:?}")]
pub struct BrokerDisableEnvError {
    /// Value read from `RUNNING_PROCESS_DISABLE`.
    pub value: String,
}

/// Errors produced by broker client helpers.
#[derive(Debug, thiserror::Error)]
pub enum BrokerClientError {
    /// Could not connect to the broker.
    #[error("failed to connect to broker: {0}")]
    BrokerConnect(io::Error),
    /// Broker negotiation succeeded but the returned backend endpoint failed.
    #[error("failed to connect to negotiated backend: {0}")]
    BackendConnect(io::Error),
    /// Frame read/write failed.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// Broker response frame was malformed.
    #[error("failed to decode broker response Frame: {0}")]
    DecodeFrame(prost::DecodeError),
    /// Broker response payload was not a valid `HelloReply`.
    #[error("failed to decode broker HelloReply: {0}")]
    DecodeHelloReply(prost::DecodeError),
    /// Broker response payload was not a valid `AdminReply`.
    #[error("failed to decode broker AdminReply: {0}")]
    DecodeAdminReply(prost::DecodeError),
    /// Broker returned an unexpected response envelope.
    #[error("unexpected broker response frame: {0}")]
    UnexpectedResponseFrame(&'static str),
    /// Broker returned `HelloReply` without a result.
    #[error("broker HelloReply did not contain a result")]
    MissingHelloReplyResult,
    /// Broker refused the Hello request.
    #[error("broker refused Hello: {reason} ({code:?}, retry_after_ms={retry_after_ms})")]
    Refused {
        /// Stable refusal code.
        code: ErrorCode,
        /// Human-readable reason.
        reason: String,
        /// Retry hint.
        retry_after_ms: u64,
    },
    /// Broker returned an empty backend endpoint.
    #[error("broker negotiated an empty backend endpoint")]
    EmptyBackendPipe,
}

impl BrokerClientError {
    /// Classify a broker refusal into a stable, matchable kind (#433 R7).
    ///
    /// Returns `Some` only for [`BrokerClientError::Refused`]; every other
    /// (transport/decoding) error returns `None`. Consumers branch on
    /// [`RefusalKind`] instead of pattern-matching the raw `i32`
    /// [`ErrorCode`], so retry/escalate decisions stay readable and survive
    /// the addition of future codes (mapped to [`RefusalKind::Other`]).
    pub fn refusal_kind(&self) -> Option<RefusalKind> {
        match self {
            BrokerClientError::Refused { code, .. } => Some(RefusalKind::from_code(*code)),
            _ => None,
        }
    }
}

/// Stable, matchable classification of a broker `HelloReply::Refused` code.
///
/// This is the consumer-facing decision surface for the broker's refusal
/// codes: the wire carries an [`ErrorCode`] `i32`, but a future broker may add
/// codes a consumer's build predates. Matching on `RefusalKind` keeps consumer
/// retry logic exhaustive and forward-compatible — any unrecognized code lands
/// in [`RefusalKind::Other`] rather than silently mismatching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalKind {
    /// The requested version is below the backend's `min_version` or otherwise
    /// not offered. Caller should upgrade/downgrade, not blindly retry.
    VersionUnsupported,
    /// The requested version is explicitly blocked (e.g. yanked). Do not retry
    /// with the same version.
    VersionBlocked,
    /// The service name is unknown to this broker. A configuration error;
    /// retrying will not help.
    ServiceUnknown,
    /// The broker is rate-limiting this peer. Honour `retry_after_ms`.
    RateLimited,
    /// The broker is shutting down. Retry against a fresh broker.
    ShuttingDown,
    /// Any other refusal code (peer rejected, internal, fd pressure, spawn
    /// failure, unspecified, or a code newer than this build understands).
    Other(ErrorCode),
}

impl RefusalKind {
    /// Map a wire [`ErrorCode`] to its [`RefusalKind`].
    pub fn from_code(code: ErrorCode) -> Self {
        match code {
            ErrorCode::ErrorVersionUnsupported => RefusalKind::VersionUnsupported,
            ErrorCode::ErrorVersionBlocked => RefusalKind::VersionBlocked,
            ErrorCode::ErrorServiceUnknown => RefusalKind::ServiceUnknown,
            ErrorCode::ErrorRateLimited => RefusalKind::RateLimited,
            ErrorCode::ErrorShuttingDown => RefusalKind::ShuttingDown,
            other => RefusalKind::Other(other),
        }
    }
}

#[cfg(test)]
mod cluster_h_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn broker_client_timeout_defaults_when_unset_or_invalid() {
        assert_eq!(
            parse_broker_client_timeout(None),
            DEFAULT_BROKER_CLIENT_TIMEOUT
        );
        assert_eq!(
            parse_broker_client_timeout(Some("nope")),
            DEFAULT_BROKER_CLIENT_TIMEOUT
        );
        assert_eq!(
            parse_broker_client_timeout(Some("0")),
            DEFAULT_BROKER_CLIENT_TIMEOUT
        );
    }

    #[test]
    fn broker_client_timeout_honors_valid_override() {
        assert_eq!(
            parse_broker_client_timeout(Some("750")),
            Duration::from_millis(750)
        );
    }

    #[test]
    fn send_admin_request_to_missing_broker_errors_promptly() {
        // A broker endpoint that does not exist must fail fast (connection
        // refused / not found), never hang, and return within the bounded
        // helper-thread deadline.
        let bogus = if cfg!(windows) {
            r"\.\pipe\running-process-broker-nonexistent-cluster-h-test"
        } else {
            "/tmp/running-process-broker-nonexistent-cluster-h-test.sock"
        };
        let start = Instant::now();
        let result = send_admin_request(bogus, AdminRequest::default());
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "send_admin_request to a missing broker took {:?}; should fail fast",
            start.elapsed()
        );
    }
}
