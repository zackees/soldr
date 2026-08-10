#![cfg(feature = "client")]

use std::path::Path;
use std::time::{Duration, Instant};

use running_process::broker::backend_lib::{
    accept_handed_off, HandedOffPayload, HandoffAcceptance, HandoffRejectionReason, HandoffToken,
    HandoffTokenStore,
};
use running_process::broker::server::handoff::{
    DuplicateHandleSuccess, HandoffAttemptDecision, HandoffAttemptFailure, HandoffAttemptInputs,
    HandoffFallbackReason, HandoffFallbackState, ScmRightsSuccess, UnixFileDescriptor,
    UnixHandoffSocket, WindowsHandleValue,
};
use running_process::broker::server::{BackendKey, BrokerInstanceKey};

#[derive(Clone, Debug, PartialEq, Eq)]
enum TransportSuccess {
    WindowsDuplicateHandle(DuplicateHandleSuccess),
    UnixScmRights(ScmRightsSuccess),
}

impl TransportSuccess {
    fn token(&self) -> HandoffToken {
        match self {
            Self::WindowsDuplicateHandle(success) => success.handoff_token,
            Self::UnixScmRights(success) => success.handoff_token,
        }
    }
}

fn token(byte: u8) -> HandoffToken {
    HandoffToken::from_bytes([byte; 16])
}

fn issue_token(store: &mut HandoffTokenStore, now: Instant, byte: u8) -> HandoffToken {
    store
        .issue_with_random128(now, || Ok(token(byte).into_bytes()))
        .unwrap()
}

fn backend_key() -> BackendKey {
    BackendKey::new(BrokerInstanceKey::Shared, "zccache", "1.11.20")
}

fn assert_silent_reconnect(
    decision: HandoffAttemptDecision,
    expected_reason: HandoffFallbackReason,
) {
    let HandoffAttemptDecision::FallbackToReconnect(fallback) = decision else {
        panic!("expected silent reconnect fallback");
    };

    assert_eq!(fallback.reason, expected_reason);
    assert!(fallback.uses_backend_reconnect());
    assert!(!fallback.sends_client_error());
}

fn accept_transport(
    pending_tokens: &mut HandoffTokenStore,
    transport: TransportSuccess,
    now: Instant,
) -> TransportSuccess {
    let expected = transport.token();
    let presented = expected.as_bytes().to_vec();
    let accepted = accept_handed_off(
        pending_tokens,
        HandedOffPayload::new(expected, presented, transport),
        now,
    )
    .into_result()
    .expect("transport success token should be accepted by backend helper");

    assert_eq!(accepted.token, expected);
    accepted.connection
}

#[test]
fn platform_transport_successes_feed_backend_acceptance_and_consume_tokens_once() {
    let now = Instant::now();
    let backend = backend_key();
    let mut fallback_state = HandoffFallbackState::new();
    let mut tokens = HandoffTokenStore::new();

    assert_eq!(
        fallback_state.should_attempt(&backend, HandoffAttemptInputs::enabled(), now),
        HandoffAttemptDecision::Attempt
    );

    let windows_token = issue_token(&mut tokens, now, 0x37);
    let windows = TransportSuccess::WindowsDuplicateHandle(DuplicateHandleSuccess::new(
        WindowsHandleValue::new(0x51),
        4242,
        windows_token,
    ));
    let accepted_windows = accept_transport(&mut tokens, windows, now);

    let TransportSuccess::WindowsDuplicateHandle(accepted_windows) = accepted_windows else {
        panic!("expected Windows DuplicateHandle success");
    };
    assert_eq!(accepted_windows.duplicated_handle.get(), 0x51);
    assert_eq!(accepted_windows.backend_pid, 4242);

    let unix_token = issue_token(&mut tokens, now + Duration::from_millis(1), 0x54);
    let unix_socket = UnixHandoffSocket::new("/tmp/running-process-handoff.sock");
    let unix = TransportSuccess::UnixScmRights(ScmRightsSuccess::new(
        UnixFileDescriptor::new(17),
        unix_socket.clone(),
        unix_token,
    ));
    let accepted_unix = accept_transport(&mut tokens, unix, now + Duration::from_millis(1));

    let TransportSuccess::UnixScmRights(accepted_unix) = accepted_unix else {
        panic!("expected Unix SCM_RIGHTS success");
    };
    assert_eq!(accepted_unix.sent_fd.raw(), 17);
    assert_eq!(accepted_unix.backend_socket, unix_socket);
    assert_eq!(tokens.pending_len(), 0);

    fallback_state.record_success(&backend);
    assert_eq!(
        fallback_state.should_attempt(&backend, HandoffAttemptInputs::enabled(), now),
        HandoffAttemptDecision::Attempt
    );

    let replay = HandedOffPayload::new(
        windows_token,
        windows_token.as_bytes().to_vec(),
        "duplicate transport replay",
    );
    let replay = accept_handed_off(&mut tokens, replay, now);
    let HandoffAcceptance::Rejected(replay) = replay else {
        panic!("expected consumed token replay to be rejected");
    };
    assert_eq!(replay.reason, HandoffRejectionReason::TokenNotPending);
}

#[test]
fn backend_rejection_maps_to_silent_reconnect_and_leaves_token_retryable() {
    let now = Instant::now();
    let backend = backend_key();
    let mut fallback_state = HandoffFallbackState::new();
    let mut tokens = HandoffTokenStore::new();
    let expected = issue_token(&mut tokens, now, 0x65);
    let wrong = token(0x66);
    let transport = TransportSuccess::UnixScmRights(ScmRightsSuccess::new(
        UnixFileDescriptor::new(18),
        UnixHandoffSocket::new("/tmp/running-process-handoff.sock"),
        expected,
    ));

    assert_eq!(
        fallback_state.should_attempt(&backend, HandoffAttemptInputs::enabled(), now),
        HandoffAttemptDecision::Attempt
    );

    let rejected = accept_handed_off(
        &mut tokens,
        HandedOffPayload::new(expected, wrong.as_bytes().to_vec(), transport),
        now,
    );
    let HandoffAcceptance::Rejected(rejected) = rejected else {
        panic!("expected mismatched token to reject the handed-off transport");
    };
    assert_eq!(rejected.reason, HandoffRejectionReason::TokenMismatch);
    assert_eq!(tokens.pending_len(), 1);

    assert_silent_reconnect(
        fallback_state.record_failed_attempt(
            backend.clone(),
            HandoffAttemptFailure::IntegrityMismatch,
            now,
        ),
        HandoffFallbackReason::IntegrityMismatch,
    );

    let retry = HandedOffPayload::new(
        expected,
        expected.as_bytes().to_vec(),
        "correctly-presented retry",
    );
    let accepted = accept_handed_off(&mut tokens, retry, now)
        .into_result()
        .expect("token mismatch must not consume the pending token");

    assert_eq!(accepted.connection, "correctly-presented retry");
    assert_eq!(tokens.pending_len(), 0);
}

#[test]
fn docs_pin_end_to_end_handoff_acceptance_evidence() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let doc_path = repo_root.join("docs/v1-handoff-optimization.md");
    let doc = std::fs::read_to_string(doc_path).unwrap();

    assert!(doc.contains("## End-to-End Acceptance Evidence"));
    assert!(doc.contains("handoff_end_to_end_acceptance"));
    assert!(doc.contains("DuplicateHandleSuccess"));
    assert!(doc.contains("ScmRightsSuccess"));
    assert!(doc.contains("accept_handed_off"));
    assert!(doc.contains("TokenMismatch"));
    assert!(doc.contains("silent reconnect fallback"));
}
