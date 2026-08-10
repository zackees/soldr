//! Backend ACK deadline handling for pending handoffs (#354, slice 2).
//!
//! A handoff negotiated at Hello time is only complete once the backend
//! acknowledges adopting the passed handle before the broker ACK deadline.
//! On expiry the handoff is abandoned, the one-time token is revoked, and
//! the `backend_pipe` reconnect path remains authoritative.

use std::time::{Duration, Instant};

use prost::Message;
use running_process::broker::backend_lib::{
    accept_handed_off, parse_handoff_token, HandedOffPayload, HandoffRejectionReason,
};
use running_process::broker::capabilities::CAP_HANDLE_PASSING;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, BrokerIsolation, Frame, FrameKind, Hello, Negotiated,
    PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    HandoffAckError, HandoffAckRegistry, HandoffAttemptFailure, HandoffFallbackReason,
    HandoffToken, HandoffTokenStore, HelloHandler, PeerIdentity, PendingHandoffBackend,
    RegisteredBackend, DEFAULT_HANDOFF_ACK_DEADLINE, DEFAULT_HANDOFF_TOKEN_TTL,
};

const BACKEND_PIPE: &str = r"\\.\pipe\rpb-v1-handoff-ack-backend";

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

fn hello(client_capabilities: u64) -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities,
        auth_token: Vec::new(),
        request_id: "req-handoff-ack".into(),
        connection_id: 0,
        peer_pid: std::process::id(),
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn negotiate(handler: &HelloHandler, client_capabilities: u64) -> Negotiated {
    let request = hello(client_capabilities);
    let frame = Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: request.encode_to_vec(),
        request_id: 11,
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

fn negotiated_token(negotiated: &Negotiated) -> HandoffToken {
    parse_handoff_token(&negotiated.handle_passed_token).unwrap()
}

#[test]
fn hello_negotiation_registers_pending_ack_entry() {
    let handler = handler();

    let negotiated = negotiate(&handler, CAP_HANDLE_PASSING);
    let token = negotiated_token(&negotiated);

    let acks = handler.handoff_ack_registry();
    assert_eq!(acks.pending_len(), 1);
    let backend = acks.pending_backend(&token).expect("entry must be visible");
    assert_eq!(backend, &PendingHandoffBackend::for_service("zccache"));
    assert_eq!(backend.backend_pid, 0, "Hello path knows only the service");
    assert!(
        acks.ack_deadline() < DEFAULT_HANDOFF_TOKEN_TTL,
        "ACK deadline must be shorter than the token TTL"
    );
}

#[test]
fn hello_without_capability_registers_no_ack_entry() {
    let handler = handler();

    let negotiated = negotiate(&handler, 0);

    assert!(negotiated.handle_passed_token.is_empty());
    assert_eq!(handler.handoff_ack_registry().pending_len(), 0);
}

#[test]
fn ack_before_deadline_completes_and_rejects_duplicate_ack() {
    let handler = handler();
    let negotiated = negotiate(&handler, CAP_HANDLE_PASSING);
    let token = negotiated_token(&negotiated);
    let now = Instant::now();

    let acknowledged = handler.acknowledge_handoff(&token, now).unwrap();
    assert_eq!(acknowledged.token, token);
    assert_eq!(
        acknowledged.backend,
        PendingHandoffBackend::for_service("zccache")
    );
    assert!(acknowledged.waited < DEFAULT_HANDOFF_ACK_DEADLINE);

    // Completed: no pending ACK entry and the one-time token is revoked.
    assert_eq!(handler.handoff_ack_registry().pending_len(), 0);
    assert_eq!(handler.handoff_token_store().pending_len(), 0);

    // A duplicate ACK must be rejected.
    assert_eq!(
        handler.acknowledge_handoff(&token, now),
        Err(HandoffAckError::TokenNotPending)
    );
}

#[test]
fn missed_deadline_expires_with_backend_ack_timeout_and_revokes_token() {
    let handler = handler().with_handoff_ack_deadline(Duration::from_millis(10));
    let negotiated = negotiate(&handler, CAP_HANDLE_PASSING);
    let token = negotiated_token(&negotiated);
    let after_deadline = Instant::now() + Duration::from_secs(1);

    let expired = handler.expire_overdue_handoffs(after_deadline);
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].token, token);
    assert_eq!(
        expired[0].backend,
        PendingHandoffBackend::for_service("zccache")
    );
    assert_eq!(
        expired[0].attempt_failure(),
        HandoffAttemptFailure::BackendAckTimeout
    );
    assert_eq!(
        expired[0].fallback_decision().reason,
        HandoffFallbackReason::BackendAckTimeout
    );

    // A late ACK is rejected and the token is fully revoked.
    assert_eq!(
        handler.acknowledge_handoff(&token, after_deadline),
        Err(HandoffAckError::TokenNotPending)
    );
    assert_eq!(handler.handoff_token_store().pending_len(), 0);

    // The revoked token can no longer be presented on the backend side.
    let rejected = accept_handed_off(
        &mut handler.handoff_token_store(),
        HandedOffPayload::new(token, negotiated.handle_passed_token.clone(), "conn-late"),
        after_deadline,
    );
    assert_eq!(
        rejected.into_result().unwrap_err().reason,
        HandoffRejectionReason::TokenNotPending
    );
}

#[test]
fn late_ack_without_sweep_reports_deadline_exceeded() {
    let mut acks = HandoffAckRegistry::with_ack_deadline(Duration::from_millis(5));
    let mut tokens = HandoffTokenStore::new();
    let issued_at = Instant::now();
    let token = tokens.issue(issued_at).unwrap();
    acks.register(
        token,
        PendingHandoffBackend::new("zccache", 4242),
        issued_at,
    );

    let late = issued_at + Duration::from_millis(50);
    let error = acks.acknowledge(&mut tokens, &token, late).unwrap_err();
    match &error {
        HandoffAckError::AckDeadlineExceeded { backend, deadline } => {
            assert_eq!(backend, &PendingHandoffBackend::new("zccache", 4242));
            assert_eq!(*deadline, Duration::from_millis(5));
        }
        other => panic!("expected AckDeadlineExceeded, got {other:?}"),
    }
    assert_eq!(
        error.attempt_failure(),
        Some(HandoffAttemptFailure::BackendAckTimeout)
    );

    // Lazy expiry also revokes the token and clears the pending entry.
    assert_eq!(acks.pending_len(), 0);
    assert_eq!(tokens.pending_len(), 0);
}

#[test]
fn expired_handoff_never_invalidates_backend_pipe_reconnect() {
    let handler = handler().with_handoff_ack_deadline(Duration::from_millis(10));

    let negotiated = negotiate(&handler, CAP_HANDLE_PASSING);
    assert_eq!(negotiated.backend_pipe, BACKEND_PIPE);

    let expired = handler.expire_overdue_handoffs(Instant::now() + Duration::from_secs(1));
    assert_eq!(expired.len(), 1);

    // Expiry maps to the silent reconnect fallback: the client keeps using
    // the already-negotiated backend_pipe and is never sent an error.
    let fallback = expired[0].fallback_decision();
    assert!(fallback.uses_backend_reconnect());
    assert!(!fallback.sends_client_error());

    // A fresh negotiation after the failure still returns the reconnect
    // endpoint, with or without handle passing.
    let after_failure = negotiate(&handler, CAP_HANDLE_PASSING);
    assert_eq!(after_failure.backend_pipe, BACKEND_PIPE);
    let without_handoff = negotiate(&handler, 0);
    assert_eq!(without_handoff.backend_pipe, BACKEND_PIPE);
}
