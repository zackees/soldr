use std::time::{Duration, Instant};

use running_process::broker::backend_lib::{
    accept_handed_off, parse_handoff_token, HandedOffPayload, HandoffAcceptance,
    HandoffRejectionReason, HandoffToken, HandoffTokenStore, HandoffTokenStoreConfig,
    HANDOFF_TOKEN_BYTES,
};

fn token(byte: u8) -> HandoffToken {
    HandoffToken::from_bytes([byte; HANDOFF_TOKEN_BYTES])
}

fn issue_token(store: &mut HandoffTokenStore, now: Instant, byte: u8) -> HandoffToken {
    store
        .issue_with_random128(now, || Ok(token(byte).into_bytes()))
        .unwrap()
}

fn assert_rejected<T>(
    acceptance: HandoffAcceptance<T>,
    reason: HandoffRejectionReason,
) -> HandedOffPayload<T> {
    let HandoffAcceptance::Rejected(rejected) = acceptance else {
        panic!("expected rejected handoff");
    };

    assert_eq!(rejected.reason, reason);
    rejected.payload
}

#[test]
fn accepts_valid_payload_and_consumes_token_once() {
    let now = Instant::now();
    let mut store = HandoffTokenStore::new();
    let expected = issue_token(&mut store, now, 1);
    let payload = HandedOffPayload::new(expected, expected.as_bytes().to_vec(), "owned-fd");

    let accepted = accept_handed_off(&mut store, payload, now)
        .into_result()
        .expect("valid payload accepted");

    assert_eq!(accepted.token, expected);
    assert_eq!(accepted.connection, "owned-fd");
    assert_eq!(store.pending_len(), 0);

    let replay = HandedOffPayload::new(expected, expected.as_bytes().to_vec(), "replay");
    let replay = assert_rejected(
        accept_handed_off(&mut store, replay, now),
        HandoffRejectionReason::TokenNotPending,
    );
    assert_eq!(replay.connection, "replay");
}

#[test]
fn token_mismatch_rejects_without_consuming_pending_token() {
    let now = Instant::now();
    let mut store = HandoffTokenStore::new();
    let expected = issue_token(&mut store, now, 2);
    let wrong = token(3);

    assert_rejected(
        accept_handed_off(
            &mut store,
            HandedOffPayload::new(expected, wrong.as_bytes().to_vec(), "wrong"),
            now,
        ),
        HandoffRejectionReason::TokenMismatch,
    );
    assert_eq!(store.pending_len(), 1);

    let accepted = accept_handed_off(
        &mut store,
        HandedOffPayload::new(expected, expected.as_bytes().to_vec(), "correct"),
        now,
    )
    .into_result()
    .expect("correct retry accepted");
    assert_eq!(accepted.connection, "correct");
}

#[test]
fn malformed_token_bytes_reject_without_consuming_pending_token() {
    let now = Instant::now();
    let mut store = HandoffTokenStore::new();
    let expected = issue_token(&mut store, now, 4);

    assert_rejected(
        accept_handed_off(
            &mut store,
            HandedOffPayload::new(expected, Vec::<u8>::new(), "missing"),
            now,
        ),
        HandoffRejectionReason::MissingToken,
    );
    assert_rejected(
        accept_handed_off(
            &mut store,
            HandedOffPayload::new(expected, vec![9; HANDOFF_TOKEN_BYTES - 1], "short"),
            now,
        ),
        HandoffRejectionReason::InvalidTokenLength {
            actual_len: HANDOFF_TOKEN_BYTES - 1,
            expected_len: HANDOFF_TOKEN_BYTES,
        },
    );
    assert_eq!(store.pending_len(), 1);

    accept_handed_off(
        &mut store,
        HandedOffPayload::new(expected, expected.as_bytes().to_vec(), "correct"),
        now,
    )
    .into_result()
    .expect("correct payload still accepted");
}

#[test]
fn expired_token_rejects_and_removes_pending_token() {
    let now = Instant::now();
    let ttl = Duration::from_millis(5);
    let later = now + ttl + Duration::from_millis(1);
    let mut store = HandoffTokenStore::with_config(HandoffTokenStoreConfig::new(1, ttl));
    let expected = issue_token(&mut store, now, 5);

    assert_rejected(
        accept_handed_off(
            &mut store,
            HandedOffPayload::new(expected, expected.as_bytes().to_vec(), "late"),
            later,
        ),
        HandoffRejectionReason::TokenExpired,
    );
    assert_eq!(store.pending_len(), 0);
}

#[test]
fn parser_accepts_only_exact_128_bit_tokens() {
    let exact = [8_u8; HANDOFF_TOKEN_BYTES];

    assert_eq!(
        parse_handoff_token(&[]),
        Err(HandoffRejectionReason::MissingToken)
    );
    assert_eq!(
        parse_handoff_token(&exact[..HANDOFF_TOKEN_BYTES - 1]),
        Err(HandoffRejectionReason::InvalidTokenLength {
            actual_len: HANDOFF_TOKEN_BYTES - 1,
            expected_len: HANDOFF_TOKEN_BYTES,
        })
    );
    assert_eq!(
        parse_handoff_token(&exact).unwrap(),
        HandoffToken::from_bytes(exact)
    );
}
