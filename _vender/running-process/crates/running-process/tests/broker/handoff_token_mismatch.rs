use std::time::{Duration, Instant};

use running_process::broker::server::{
    HandoffToken, HandoffTokenError, HandoffTokenStore, HandoffTokenStoreConfig,
    HANDOFF_TOKEN_BYTES,
};

fn token(byte: u8) -> HandoffToken {
    HandoffToken::from_bytes([byte; HANDOFF_TOKEN_BYTES])
}

#[test]
fn generated_handoff_token_is_128_bits() {
    let generated = HandoffToken::generate().unwrap();

    assert_eq!(generated.as_bytes().len(), HANDOFF_TOKEN_BYTES);
}

#[test]
fn token_mismatch_does_not_consume_pending_token() {
    let now = Instant::now();
    let mut store =
        HandoffTokenStore::with_config(HandoffTokenStoreConfig::new(2, Duration::from_secs(30)));
    let expected = store
        .issue_with_random128(now, || Ok(token(1).into_bytes()))
        .unwrap();
    let wrong = token(2);

    assert_eq!(
        store.consume_matching(&expected, &wrong, now),
        Err(HandoffTokenError::TokenMismatch)
    );
    assert_eq!(store.pending_len(), 1);

    store.consume_matching(&expected, &expected, now).unwrap();
    assert_eq!(
        store.consume_matching(&expected, &expected, now),
        Err(HandoffTokenError::TokenNotPending)
    );
}

#[test]
fn pending_tokens_are_capacity_bounded_until_consumed() {
    let now = Instant::now();
    let mut store =
        HandoffTokenStore::with_config(HandoffTokenStoreConfig::new(1, Duration::from_secs(30)));
    let first = store
        .issue_with_random128(now, || Ok(token(3).into_bytes()))
        .unwrap();

    assert_eq!(
        store.issue_with_random128(now, || Ok(token(4).into_bytes())),
        Err(HandoffTokenError::PendingLimitReached {
            max_pending_tokens: 1,
        })
    );

    store.consume_matching(&first, &first, now).unwrap();
    let second = store
        .issue_with_random128(now, || Ok(token(4).into_bytes()))
        .unwrap();
    store.consume_matching(&second, &second, now).unwrap();
}

#[test]
fn expired_tokens_are_rejected_and_pruned_for_capacity() {
    let now = Instant::now();
    let ttl = Duration::from_millis(5);
    let mut store = HandoffTokenStore::with_config(HandoffTokenStoreConfig::new(1, ttl));
    let expired = store
        .issue_with_random128(now, || Ok(token(5).into_bytes()))
        .unwrap();
    let later = now + ttl + Duration::from_millis(1);

    assert_eq!(
        store.consume_matching(&expired, &expired, later),
        Err(HandoffTokenError::TokenExpired)
    );
    assert_eq!(store.pending_len(), 0);

    let fresh = store
        .issue_with_random128(later, || Ok(token(6).into_bytes()))
        .unwrap();
    store.consume_matching(&fresh, &fresh, later).unwrap();
}

#[test]
fn random_collisions_are_bounded() {
    let now = Instant::now();
    let mut store = HandoffTokenStore::with_config(
        HandoffTokenStoreConfig::new(2, Duration::from_secs(30)).with_collision_attempts(3),
    );
    let first = store
        .issue_with_random128(now, || Ok(token(7).into_bytes()))
        .unwrap();

    assert_eq!(
        store.issue_with_random128(now, || Ok(first.into_bytes())),
        Err(HandoffTokenError::CollisionExhausted { attempts: 3 })
    );
}
