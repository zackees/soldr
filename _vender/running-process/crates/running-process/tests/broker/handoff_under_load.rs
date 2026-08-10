#![cfg(feature = "client")]

use std::time::{Duration, Instant};

use running_process::broker::server::{
    HandoffAttemptDecision, HandoffFallbackReason, PendingHandoffOverflow, PendingHandoffQueue,
    PendingHandoffQueueConfig,
};

fn assert_overflow_falls_back_safely(overflow: PendingHandoffOverflow, capacity: usize) {
    assert_eq!(overflow.max_pending_handoffs, capacity);
    assert_eq!(
        overflow.fallback_reason(),
        HandoffFallbackReason::FdPressureDisabled
    );
    assert!(overflow.is_fallback_safe());

    let HandoffAttemptDecision::FallbackToReconnect(fallback) =
        overflow.fallback_attempt_decision()
    else {
        panic!("expected reconnect fallback");
    };
    assert_eq!(fallback.reason, HandoffFallbackReason::FdPressureDisabled);
    assert!(fallback.uses_backend_reconnect());
    assert!(!fallback.sends_client_error());
}

#[test]
fn hundred_simulated_handoffs_cannot_grow_backlog_without_bound() {
    let now = Instant::now();
    let capacity = 8;
    let mut queue = PendingHandoffQueue::with_config(PendingHandoffQueueConfig::new(
        capacity,
        Duration::from_secs(1),
    ));
    let mut overflow_count = 0;

    for handoff_id in 0..100 {
        match queue.enqueue(handoff_id, now + Duration::from_micros(handoff_id)) {
            Ok(()) => {}
            Err(overflow) => {
                overflow_count += 1;
                assert_overflow_falls_back_safely(overflow, capacity);
            }
        }

        assert!(queue.pending_len() <= capacity);
    }

    assert_eq!(queue.pending_len(), capacity);
    assert_eq!(overflow_count, 100 - capacity);
}

#[test]
fn pending_handoffs_dequeue_in_fifo_order_after_overflow() {
    let now = Instant::now();
    let mut queue =
        PendingHandoffQueue::with_config(PendingHandoffQueueConfig::new(3, Duration::from_secs(1)));

    queue.enqueue(10, now).unwrap();
    queue.enqueue(20, now + Duration::from_micros(1)).unwrap();
    queue.enqueue(30, now + Duration::from_micros(2)).unwrap();
    assert_overflow_falls_back_safely(
        queue
            .enqueue(40, now + Duration::from_micros(3))
            .expect_err("fourth handoff should overflow"),
        3,
    );

    let drain_at = now + Duration::from_millis(1);
    assert_eq!(queue.dequeue(drain_at), Some(10));
    assert_eq!(queue.dequeue(drain_at), Some(20));
    assert_eq!(queue.dequeue(drain_at), Some(30));
    assert_eq!(queue.dequeue(drain_at), None);
}

#[test]
fn expired_pending_handoffs_free_capacity_without_client_error() {
    let now = Instant::now();
    let ttl = Duration::from_millis(5);
    let mut queue = PendingHandoffQueue::with_config(PendingHandoffQueueConfig::new(2, ttl));

    queue.enqueue("first", now).unwrap();
    queue
        .enqueue("second", now + Duration::from_millis(1))
        .unwrap();
    assert_overflow_falls_back_safely(
        queue
            .enqueue("overflow", now + Duration::from_millis(2))
            .expect_err("third handoff should overflow before expiry"),
        2,
    );

    assert_eq!(queue.expire(now + ttl + Duration::from_millis(2)), 2);
    assert_eq!(queue.pending_len(), 0);

    queue
        .enqueue("fresh", now + ttl + Duration::from_millis(2))
        .unwrap();
    assert_eq!(
        queue.dequeue(now + ttl + Duration::from_millis(3)),
        Some("fresh")
    );
    assert!(queue.is_empty());
}
