//! Platform-neutral tests for the Unix `SCM_RIGHTS` handoff orchestration
//! state machine (#354, slice 4).
//!
//! These tests inject a mock send transport and an in-process
//! [`UnixHandoffAckWait`] so the full sequence — send, await ACK,
//! acknowledge — and every fallback path run on every platform, including
//! Windows.

use std::time::{Duration, Instant};

use prost::Message;
use running_process::broker::backend_lib::{accept_handed_off, HandedOffPayload};
use running_process::broker::capabilities::CAP_HANDLE_PASSING;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, BrokerIsolation, Frame, FrameKind, Hello, Negotiated,
    PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::handoff::{
    execute_unix_handoff, execute_unix_handoff_with_transport, HandoffAckError, HandoffAckRegistry,
    HandoffDeliveryError, HandoffFallbackReason, HandoffToken, HandoffTokenStore,
    PendingHandoffBackend, ScmRightsError, ScmRightsSuccess, UnixFileDescriptor,
    UnixHandoffAckWait, UnixHandoffOutcome, UnixHandoffRequest, UnixHandoffSocket,
    UnixHandoffStage,
};
use running_process::broker::server::{HelloHandler, PeerIdentity, RegisteredBackend};

const BACKEND_PID: u32 = 5252;
const BACKEND_PIPE: &str = "/run/rpb/v1/handoff-unix-orchestrate-backend.sock";

fn token(byte: u8) -> HandoffToken {
    HandoffToken::from_bytes([byte; 16])
}

fn issue_registered_token(
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    now: Instant,
    byte: u8,
) -> HandoffToken {
    let issued = tokens
        .issue_with_random128(now, || Ok(token(byte).into_bytes()))
        .unwrap();
    acks.register(
        issued,
        PendingHandoffBackend::new("zccache", BACKEND_PID),
        now,
    );
    issued
}

fn backend_socket() -> UnixHandoffSocket {
    UnixHandoffSocket::new("/run/rpb/v1/handoff-unix-orchestrate.sock")
}

fn request(issued: HandoffToken) -> UnixHandoffRequest {
    UnixHandoffRequest::new(UnixFileDescriptor::new(7), backend_socket(), issued)
}

fn sent(issued: HandoffToken) -> ScmRightsSuccess {
    ScmRightsSuccess::new(UnixFileDescriptor::new(7), backend_socket(), issued)
}

/// Scripted in-process ACK channel recording every orchestrator call.
struct MockAckWait {
    ack_result: Result<Duration, HandoffDeliveryError>,
    ack_waits: Vec<HandoffToken>,
}

impl MockAckWait {
    fn succeeding() -> Self {
        Self {
            ack_result: Ok(Duration::ZERO),
            ack_waits: Vec::new(),
        }
    }

    fn timing_out(detail: &str) -> Self {
        Self {
            ack_result: Err(HandoffDeliveryError::AckNotObserved {
                detail: detail.into(),
            }),
            ack_waits: Vec::new(),
        }
    }

    fn acking_after(delay: Duration) -> Self {
        Self {
            ack_result: Ok(delay),
            ack_waits: Vec::new(),
        }
    }
}

impl UnixHandoffAckWait for MockAckWait {
    fn await_backend_ack(
        &mut self,
        token: &HandoffToken,
        _deadline: Instant,
    ) -> Result<Instant, HandoffDeliveryError> {
        self.ack_waits.push(*token);
        self.ack_result
            .clone()
            .map(|delay| Instant::now().checked_add(delay).unwrap())
    }
}

fn assert_abandoned(
    tokens: &HandoffTokenStore,
    acks: &HandoffAckRegistry,
    outcome: &UnixHandoffOutcome,
    stage: UnixHandoffStage,
    reason: HandoffFallbackReason,
) {
    let fallback = outcome.fallback().expect("handoff must fall back");
    assert_eq!(fallback.stage, stage);
    assert_eq!(fallback.decision.reason, reason);
    assert!(fallback.decision.uses_backend_reconnect());
    assert!(!fallback.decision.sends_client_error());
    assert_eq!(
        fallback.broker_fd,
        UnixFileDescriptor::new(7),
        "broker keeps ownership of its own fd"
    );
    assert_eq!(tokens.pending_len(), 0, "token must be revoked");
    assert_eq!(acks.pending_len(), 0, "pending ACK entry must be removed");
}

#[test]
fn successful_unix_orchestration_completes_and_consumes_token_once() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x31);
    let mut ack_wait = MockAckWait::succeeding();

    let outcome = execute_unix_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        |attempt| {
            assert_eq!(attempt.fd, UnixFileDescriptor::new(7));
            assert_eq!(attempt.backend_socket, backend_socket());
            assert_eq!(attempt.handoff_token, issued);
            Ok(sent(issued))
        },
        &mut ack_wait,
    );

    let UnixHandoffOutcome::Completed(completed) = outcome else {
        panic!("expected completed handoff, got {outcome:?}");
    };
    assert_eq!(completed.sent, sent(issued));
    assert_eq!(completed.acknowledged.token, issued);
    assert_eq!(
        completed.acknowledged.backend,
        PendingHandoffBackend::new("zccache", BACKEND_PID)
    );
    assert_eq!(ack_wait.ack_waits, vec![issued]);

    // Token consumed exactly once: registry and store are empty, and both a
    // duplicate ACK and a backend-side replay are rejected.
    assert_eq!(tokens.pending_len(), 0);
    assert_eq!(acks.pending_len(), 0);
    assert_eq!(
        acks.acknowledge(&mut tokens, &issued, Instant::now()),
        Err(HandoffAckError::TokenNotPending)
    );
    let replay = accept_handed_off(
        &mut tokens,
        HandedOffPayload::new(issued, issued.as_bytes().to_vec(), "replayed-conn"),
        Instant::now(),
    );
    assert!(replay.is_rejected());
}

#[test]
fn send_failure_falls_back_and_revokes_token_without_ack_wait() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x32);
    let mut ack_wait = MockAckWait::succeeding();

    let outcome = execute_unix_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        |attempt| {
            Err(ScmRightsError::BackendSocketUnavailable {
                socket: attempt.backend_socket.path.clone(),
            })
        },
        &mut ack_wait,
    );

    assert_abandoned(
        &tokens,
        &acks,
        &outcome,
        UnixHandoffStage::Send,
        HandoffFallbackReason::BackendAckTimeout,
    );
    let fallback = outcome.fallback().unwrap();
    assert!(
        !fallback.fd_reached_backend,
        "nothing was sent, so no duplicate exists in the backend"
    );
    assert!(ack_wait.ack_waits.is_empty(), "no ACK wait without a send");
}

#[test]
fn send_permission_denied_maps_to_permission_denied_fallback() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x33);
    let mut ack_wait = MockAckWait::succeeding();

    let outcome = execute_unix_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        |attempt| {
            Err(ScmRightsError::PermissionDenied {
                fd: attempt.fd.raw(),
                socket: attempt.backend_socket.path.clone(),
            })
        },
        &mut ack_wait,
    );

    assert_abandoned(
        &tokens,
        &acks,
        &outcome,
        UnixHandoffStage::Send,
        HandoffFallbackReason::PermissionDenied,
    );
}

#[test]
fn ack_timeout_falls_back_and_records_backend_held_duplicate() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x34);
    let mut ack_wait = MockAckWait::timing_out("no ACK before deadline");

    let outcome = execute_unix_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        |_attempt| Ok(sent(issued)),
        &mut ack_wait,
    );

    assert_abandoned(
        &tokens,
        &acks,
        &outcome,
        UnixHandoffStage::AwaitAck,
        HandoffFallbackReason::BackendAckTimeout,
    );
    let fallback = outcome.fallback().unwrap();
    assert!(
        fallback.fd_reached_backend,
        "the send succeeded, so the backend holds a duplicate"
    );

    // A late ACK after the fallback is rejected: the token was revoked.
    assert_eq!(
        acks.acknowledge(&mut tokens, &issued, Instant::now()),
        Err(HandoffAckError::TokenNotPending)
    );
}

#[test]
fn ack_observed_past_registered_deadline_is_rejected_by_registry() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::with_ack_deadline(Duration::from_millis(5));
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x35);
    // The ACK channel reports an ACK, but only after the registered
    // broker-side deadline; the registry must reject it.
    let mut ack_wait = MockAckWait::acking_after(Duration::from_secs(60));

    let outcome = execute_unix_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        |_attempt| Ok(sent(issued)),
        &mut ack_wait,
    );

    assert_abandoned(
        &tokens,
        &acks,
        &outcome,
        UnixHandoffStage::Acknowledge,
        HandoffFallbackReason::BackendAckTimeout,
    );
    assert!(outcome.fallback().unwrap().fd_reached_backend);
}

#[test]
fn production_transport_with_unreachable_socket_falls_back_without_panic() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = tokens
        .issue_with_random128(now, || Ok(token(0x36).into_bytes()))
        .unwrap();
    acks.register(
        issued,
        PendingHandoffBackend::new("zccache", BACKEND_PID),
        now,
    );
    let mut ack_wait = MockAckWait::succeeding();

    // Real transport: unsupported platform off Unix, unreachable socket on
    // Unix. Either way the outcome is a non-panicking reconnect fallback
    // with the token revoked.
    let outcome = execute_unix_handoff(
        &mut tokens,
        &mut acks,
        &UnixHandoffRequest::new(
            UnixFileDescriptor::new(7),
            UnixHandoffSocket::new("/nonexistent/rpb/handoff-unix-orchestrate-missing.sock"),
            issued,
        ),
        &mut ack_wait,
    );

    let fallback = outcome.fallback().expect("handoff must fall back");
    assert_eq!(fallback.stage, UnixHandoffStage::Send);
    assert!(!fallback.fd_reached_backend);
    assert!(fallback.decision.uses_backend_reconnect());
    assert!(!fallback.decision.sends_client_error());
    assert_eq!(tokens.pending_len(), 0);
    assert_eq!(acks.pending_len(), 0);
    assert!(ack_wait.ack_waits.is_empty());
}

// --- regression guard: failed handoff keeps backend_pipe reconnect usable ---

fn service_definition() -> ServiceDefinition {
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: "/usr/local/bin/zccache".into(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: "/opt/zccache/versions".into(),
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn handler() -> HelloHandler {
    HelloHandler::new()
        .with_backend(RegisteredBackend {
            service_definition: service_definition(),
            daemon_version: "1.11.20".into(),
            backend_pipe: BACKEND_PIPE.into(),
            server_capabilities: 0,
        })
        .unwrap()
}

fn negotiate(handler: &HelloHandler, client_capabilities: u64) -> Negotiated {
    let hello = Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities,
        auth_token: Vec::new(),
        request_id: "req-handoff-unix-orchestrate".into(),
        connection_id: 0,
        peer_pid: std::process::id(),
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    };
    let frame = Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: hello.encode_to_vec(),
        request_id: 41,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };
    let peer = PeerIdentity {
        pid: std::process::id(),
        uid_or_sid: "test-peer".into(),
    };
    match handler.handle_frame(frame, peer).result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => negotiated,
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn failed_unix_orchestration_leaves_backend_pipe_reconnect_path_usable() {
    let handler = handler();
    let negotiated = negotiate(&handler, CAP_HANDLE_PASSING);
    assert_eq!(negotiated.backend_pipe, BACKEND_PIPE);
    let issued =
        running_process::broker::backend_lib::parse_handoff_token(&negotiated.handle_passed_token)
            .unwrap();

    // Orchestrate against the handler-owned stores; the send fails.
    let mut ack_wait = MockAckWait::succeeding();
    let outcome = {
        // Lock order: ACK registry, then token store (matches HelloHandler).
        let mut acks = handler.handoff_ack_registry();
        let mut tokens = handler.handoff_token_store();
        execute_unix_handoff_with_transport(
            &mut tokens,
            &mut acks,
            &request(issued),
            |attempt| {
                Err(ScmRightsError::SendFailed {
                    fd: attempt.fd.raw(),
                    socket: attempt.backend_socket.path.clone(),
                    raw_os_error: Some(libc_epipe()),
                })
            },
            &mut ack_wait,
        )
    };
    let fallback = outcome.fallback().expect("handoff must fall back");
    assert_eq!(fallback.stage, UnixHandoffStage::Send);
    assert_eq!(
        fallback.decision.reason,
        HandoffFallbackReason::BackendAckTimeout
    );
    assert!(fallback.decision.uses_backend_reconnect());

    // The revoked token can never be presented on the backend side.
    let rejected = accept_handed_off(
        &mut handler.handoff_token_store(),
        HandedOffPayload::new(issued, negotiated.handle_passed_token.clone(), "late-conn"),
        Instant::now(),
    );
    assert!(rejected.is_rejected());

    // The reconnect path is fully usable: fresh negotiations still return
    // the backend_pipe endpoint, with and without handle passing, and a new
    // handoff token can be issued for the next attempt.
    let retry = negotiate(&handler, CAP_HANDLE_PASSING);
    assert_eq!(retry.backend_pipe, BACKEND_PIPE);
    assert!(!retry.handle_passed_token.is_empty());
    let plain = negotiate(&handler, 0);
    assert_eq!(plain.backend_pipe, BACKEND_PIPE);
    assert!(plain.handle_passed_token.is_empty());
}

/// EPIPE without depending on libc on non-Unix targets.
fn libc_epipe() -> i32 {
    32
}
