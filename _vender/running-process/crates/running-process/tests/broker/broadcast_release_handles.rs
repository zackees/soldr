#![cfg(feature = "client")]

use std::time::Duration;

use running_process::broker::server::{
    BackendKey, BroadcastBackend, BroadcastBackendResponse, BroadcastFailureReason,
    BroadcastOperation, BroadcastPolicy, BrokerInstanceKey, LifecycleBroadcastModel, QuiesceReason,
    DEFAULT_BROADCAST_ACK_TIMEOUT,
};

fn key(service_name: &str, version: &str) -> BackendKey {
    BackendKey::new(BrokerInstanceKey::Shared, service_name, version)
}

#[test]
fn broadcast_release_handles_reaches_all_live_backends() {
    let mut model = LifecycleBroadcastModel::new();
    let zccache = key("zccache", "1.11.20");
    let soldr = key("soldr", "0.7.0");
    let operation = BroadcastOperation::release_handles("cache-root");

    model.register_backend(BroadcastBackend::live(zccache.clone()));
    model.register_backend(BroadcastBackend::live(soldr.clone()));

    let result = model.broadcast(operation.clone());

    assert_eq!(result.operation, operation.clone());
    assert_eq!(result.sent_count(), 2);
    assert!(result.all_live_backends_acked());
    assert_eq!(
        result.acks.iter().map(|ack| &ack.key).collect::<Vec<_>>(),
        vec![&zccache, &soldr]
    );
    assert!(result.timeouts.is_empty());
    assert!(result.failures.is_empty());
    assert!(result.skipped_dead.is_empty());
    assert_eq!(
        model.backend(&zccache).unwrap().received_operations(),
        std::slice::from_ref(&operation)
    );
    assert_eq!(
        model.backend(&soldr).unwrap().received_operations(),
        &[operation]
    );
}

#[test]
fn broadcast_release_handles_skips_dead_backends_and_reports_timeouts() {
    let mut model =
        LifecycleBroadcastModel::with_policy(BroadcastPolicy::new(Duration::from_millis(250)));
    let acked = key("zccache", "1.11.20");
    let slow = key("soldr", "0.7.0");
    let dead = key("fbuild", "2.0.0");
    let operation = BroadcastOperation::release_handles("cache-root");

    model.register_backend(BroadcastBackend::live(acked.clone()));
    model.register_backend(
        BroadcastBackend::live(slow.clone()).with_response(BroadcastBackendResponse::Timeout),
    );
    model.register_backend(BroadcastBackend::dead(dead.clone()));

    let result = model.broadcast(operation.clone());

    assert_eq!(result.sent_count(), 2);
    assert!(!result.all_live_backends_acked());
    assert_eq!(result.acks.len(), 1);
    assert_eq!(result.acks[0].key, acked);
    assert_eq!(result.timeouts.len(), 1);
    assert_eq!(result.timeouts[0].key, slow.clone());
    assert_eq!(result.timeouts[0].timeout, Duration::from_millis(250));
    assert!(result.failures.is_empty());
    assert_eq!(result.skipped_dead, vec![dead.clone()]);
    assert_eq!(
        model.backend(&slow).unwrap().received_operations(),
        &[operation]
    );
    assert!(model
        .backend(&dead)
        .unwrap()
        .received_operations()
        .is_empty());
}

#[test]
fn broadcast_release_handles_keeps_broadcasting_after_backend_failure() {
    let mut model = LifecycleBroadcastModel::new();
    let failed = key("zccache", "1.11.20");
    let acked = key("soldr", "0.7.0");

    model.register_backend(BroadcastBackend::live(failed.clone()).with_response(
        BroadcastBackendResponse::Failure(BroadcastFailureReason::BackendError),
    ));
    model.register_backend(BroadcastBackend::live(acked.clone()));

    let result = model.release_handles_under_path("cache-root");

    assert_eq!(result.sent_count(), 2);
    assert_eq!(result.failures.len(), 1);
    assert_eq!(result.failures[0].key, failed);
    assert_eq!(
        result.failures[0].reason,
        BroadcastFailureReason::BackendError
    );
    assert_eq!(result.acks.len(), 1);
    assert_eq!(result.acks[0].key, acked);
}

#[test]
fn broadcast_quiesce_uses_same_lifecycle_fanout_model() {
    let mut model = LifecycleBroadcastModel::new();
    let backend = key("zccache", "1.11.20");
    let operation = BroadcastOperation::quiesce(QuiesceReason::IdleTimeout);

    model.register_backend(BroadcastBackend::live(backend.clone()));

    let result = model.quiesce(QuiesceReason::IdleTimeout);

    assert_eq!(result.operation, operation.clone());
    assert_eq!(result.sent_count(), 1);
    assert!(result.all_live_backends_acked());
    assert_eq!(
        model.backend(&backend).unwrap().received_operations(),
        &[operation]
    );
}

#[test]
fn default_broadcast_ack_timeout_is_five_seconds() {
    assert_eq!(DEFAULT_BROADCAST_ACK_TIMEOUT, Duration::from_secs(5));
    assert_eq!(
        BroadcastPolicy::default().ack_timeout,
        DEFAULT_BROADCAST_ACK_TIMEOUT
    );
    assert_eq!(
        BroadcastPolicy::new(Duration::ZERO).ack_timeout,
        Duration::from_millis(1)
    );
}
