//! Platform-neutral tests for the Windows handoff orchestration state
//! machine (#354, slice 3).
//!
//! These tests inject a mock duplication transport and an in-process
//! [`HandoffDelivery`] so the full sequence — duplicate, deliver, await
//! ACK, acknowledge — and every fallback path run on every platform.

use std::time::{Duration, Instant};

use prost::Message;
use running_process::broker::backend_lib::{accept_handed_off, HandedOffPayload};
use running_process::broker::capabilities::CAP_HANDLE_PASSING;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, BrokerIsolation, Frame, FrameKind, Hello, Negotiated,
    PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::handoff::{
    execute_windows_handoff, execute_windows_handoff_with_transport, DuplicateHandleError,
    DuplicateHandleSuccess, HandoffAckError, HandoffAckRegistry, HandoffDelivery,
    HandoffDeliveryError, HandoffFallbackReason, HandoffToken, HandoffTokenStore,
    PendingHandoffBackend, WindowsHandleValue, WindowsHandoffOutcome, WindowsHandoffRequest,
    WindowsHandoffStage,
};
use running_process::broker::server::{HelloHandler, PeerIdentity, RegisteredBackend};

const BACKEND_PID: u32 = 4242;
const BACKEND_PIPE: &str = r"\\.\pipe\rpb-v1-handoff-orchestrate-backend";

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

fn request(issued: HandoffToken) -> WindowsHandoffRequest {
    WindowsHandoffRequest::new(WindowsHandleValue::new(0x51), BACKEND_PID, issued)
}

fn duplicated(issued: HandoffToken) -> DuplicateHandleSuccess {
    DuplicateHandleSuccess::new(WindowsHandleValue::new(0xB0B), BACKEND_PID, issued)
}

/// Scripted in-process delivery channel recording every orchestrator call.
struct MockDelivery {
    deliver_result: Result<(), HandoffDeliveryError>,
    ack_result: Result<Duration, HandoffDeliveryError>,
    delivered: Vec<(WindowsHandleValue, HandoffToken)>,
    ack_waits: Vec<HandoffToken>,
}

impl MockDelivery {
    fn succeeding() -> Self {
        Self {
            deliver_result: Ok(()),
            ack_result: Ok(Duration::ZERO),
            delivered: Vec::new(),
            ack_waits: Vec::new(),
        }
    }

    fn failing_delivery(detail: &str) -> Self {
        Self {
            deliver_result: Err(HandoffDeliveryError::DeliveryFailed {
                detail: detail.into(),
            }),
            ..Self::succeeding()
        }
    }

    fn timing_out(detail: &str) -> Self {
        Self {
            ack_result: Err(HandoffDeliveryError::AckNotObserved {
                detail: detail.into(),
            }),
            ..Self::succeeding()
        }
    }

    fn acking_after(delay: Duration) -> Self {
        Self {
            ack_result: Ok(delay),
            ..Self::succeeding()
        }
    }
}

impl HandoffDelivery for MockDelivery {
    fn deliver(
        &mut self,
        handle: WindowsHandleValue,
        token: &HandoffToken,
    ) -> Result<(), HandoffDeliveryError> {
        self.delivered.push((handle, *token));
        self.deliver_result.clone()
    }

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
    outcome: &WindowsHandoffOutcome,
    stage: WindowsHandoffStage,
    reason: HandoffFallbackReason,
) {
    let fallback = outcome.fallback().expect("handoff must fall back");
    assert_eq!(fallback.stage, stage);
    assert_eq!(fallback.decision.reason, reason);
    assert!(fallback.decision.uses_backend_reconnect());
    assert!(!fallback.decision.sends_client_error());
    assert_eq!(tokens.pending_len(), 0, "token must be revoked");
    assert_eq!(acks.pending_len(), 0, "pending ACK entry must be removed");
}

#[test]
fn successful_orchestration_completes_and_consumes_token_once() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x21);
    let mut delivery = MockDelivery::succeeding();

    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        |attempt| {
            assert_eq!(attempt.backend_pid, BACKEND_PID);
            assert_eq!(attempt.handoff_token, issued);
            Ok(duplicated(issued))
        },
        &mut delivery,
    );

    let WindowsHandoffOutcome::Completed(completed) = outcome else {
        panic!("expected completed handoff, got {outcome:?}");
    };
    assert_eq!(completed.duplicated, duplicated(issued));
    assert_eq!(completed.acknowledged.token, issued);
    assert_eq!(
        completed.acknowledged.backend,
        PendingHandoffBackend::new("zccache", BACKEND_PID)
    );

    // Handle value and token were delivered before the ACK wait.
    assert_eq!(
        delivery.delivered,
        vec![(WindowsHandleValue::new(0xB0B), issued)]
    );
    assert_eq!(delivery.ack_waits, vec![issued]);

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
fn transport_failure_falls_back_and_revokes_token_without_delivery() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x22);
    let mut delivery = MockDelivery::succeeding();

    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        |_attempt| {
            Err(DuplicateHandleError::PermissionDenied {
                backend_pid: BACKEND_PID,
            })
        },
        &mut delivery,
    );

    assert_abandoned(
        &tokens,
        &acks,
        &outcome,
        WindowsHandoffStage::Duplicate,
        HandoffFallbackReason::PermissionDenied,
    );
    let fallback = outcome.fallback().unwrap();
    assert_eq!(
        fallback.leaked_backend_handle, None,
        "nothing was duplicated, so nothing can leak"
    );
    assert!(delivery.delivered.is_empty(), "delivery must not run");
    assert!(
        delivery.ack_waits.is_empty(),
        "no ACK wait without delivery"
    );
}

#[test]
fn delivery_failure_falls_back_revokes_token_and_records_leaked_handle() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x23);
    let mut delivery = MockDelivery::failing_delivery("backend control channel closed");

    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        |_attempt| Ok(duplicated(issued)),
        &mut delivery,
    );

    assert_abandoned(
        &tokens,
        &acks,
        &outcome,
        WindowsHandoffStage::Deliver,
        HandoffFallbackReason::BackendAckTimeout,
    );
    let fallback = outcome.fallback().unwrap();
    // The handle is already in the backend's handle table; the broker cannot
    // close it and must report the leak honestly.
    assert_eq!(
        fallback.leaked_backend_handle,
        Some(WindowsHandleValue::new(0xB0B))
    );
    assert!(fallback.detail.contains("backend control channel closed"));
    assert!(delivery.ack_waits.is_empty(), "no ACK wait after failure");
}

#[test]
fn ack_timeout_falls_back_with_backend_ack_timeout() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x24);
    let mut delivery = MockDelivery::timing_out("no ACK before deadline");

    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        |_attempt| Ok(duplicated(issued)),
        &mut delivery,
    );

    assert_abandoned(
        &tokens,
        &acks,
        &outcome,
        WindowsHandoffStage::AwaitAck,
        HandoffFallbackReason::BackendAckTimeout,
    );
    assert_eq!(
        outcome.fallback().unwrap().leaked_backend_handle,
        Some(WindowsHandleValue::new(0xB0B))
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
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x25);
    // The delivery channel reports an ACK, but only after the registered
    // broker-side deadline; the registry must reject it.
    let mut delivery = MockDelivery::acking_after(Duration::from_secs(60));

    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        |_attempt| Ok(duplicated(issued)),
        &mut delivery,
    );

    assert_abandoned(
        &tokens,
        &acks,
        &outcome,
        WindowsHandoffStage::Acknowledge,
        HandoffFallbackReason::BackendAckTimeout,
    );
}

#[test]
fn production_transport_with_unreachable_backend_falls_back_without_panic() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = tokens
        .issue_with_random128(now, || Ok(token(0x26).into_bytes()))
        .unwrap();
    acks.register(issued, PendingHandoffBackend::new("zccache", u32::MAX), now);
    let mut delivery = MockDelivery::succeeding();

    // Real transport: unsupported platform off Windows, unopenable pid on
    // Windows. Either way the outcome is a non-panicking reconnect fallback
    // with the token revoked.
    let outcome = execute_windows_handoff(
        &mut tokens,
        &mut acks,
        &WindowsHandoffRequest::new(WindowsHandleValue::new(0x51), u32::MAX, issued),
        &mut delivery,
    );

    let fallback = outcome.fallback().expect("handoff must fall back");
    assert_eq!(fallback.stage, WindowsHandoffStage::Duplicate);
    assert_eq!(fallback.leaked_backend_handle, None);
    assert!(fallback.decision.uses_backend_reconnect());
    assert!(!fallback.decision.sends_client_error());
    assert_eq!(tokens.pending_len(), 0);
    assert_eq!(acks.pending_len(), 0);
    assert!(delivery.delivered.is_empty());
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
        request_id: "req-handoff-orchestrate".into(),
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
        request_id: 31,
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
fn failed_orchestration_leaves_backend_pipe_reconnect_path_usable() {
    let handler = handler();
    let negotiated = negotiate(&handler, CAP_HANDLE_PASSING);
    assert_eq!(negotiated.backend_pipe, BACKEND_PIPE);
    let issued =
        running_process::broker::backend_lib::parse_handoff_token(&negotiated.handle_passed_token)
            .unwrap();

    // Orchestrate against the handler-owned stores; the transport fails.
    let mut delivery = MockDelivery::succeeding();
    let outcome = {
        // Lock order: ACK registry, then token store (matches HelloHandler).
        let mut acks = handler.handoff_ack_registry();
        let mut tokens = handler.handoff_token_store();
        execute_windows_handoff_with_transport(
            &mut tokens,
            &mut acks,
            &WindowsHandoffRequest::new(WindowsHandleValue::new(0x51), BACKEND_PID, issued),
            |_attempt| {
                Err(DuplicateHandleError::IntegrityMismatch {
                    backend_pid: BACKEND_PID,
                })
            },
            &mut delivery,
        )
    };
    let fallback = outcome.fallback().expect("handoff must fall back");
    assert_eq!(
        fallback.decision.reason,
        HandoffFallbackReason::IntegrityMismatch
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
