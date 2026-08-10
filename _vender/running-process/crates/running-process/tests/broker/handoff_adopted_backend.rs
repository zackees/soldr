#![cfg(feature = "client")]

use std::path::Path;
use std::time::Instant;

use running_process::broker::server::{
    BackendKey, BrokerInstanceKey, HandoffAttemptDecision, HandoffAttemptInputs,
    HandoffFallbackReason, HandoffFallbackState,
};

fn key(version: &str) -> BackendKey {
    BackendKey::new(BrokerInstanceKey::Shared, "zccache", version)
}

#[test]
fn adopted_existing_backends_always_use_reconnect_fallback() {
    let now = Instant::now();
    let backend = key("1.11.20");
    let mut state = HandoffFallbackState::new();

    let HandoffAttemptDecision::FallbackToReconnect(fallback) =
        state.should_attempt(&backend, HandoffAttemptInputs::adopted_backend(true), now)
    else {
        panic!("adopted backend must not attempt handoff");
    };

    assert_eq!(fallback.reason, HandoffFallbackReason::AdoptedBackend);
    assert!(fallback.uses_backend_reconnect());
    assert!(!fallback.sends_client_error());
}

#[test]
fn handoff_docs_cover_adopted_backend_empty_token_fallback() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let doc_path = repo_root.join("docs/v1-handoff-optimization.md");
    let doc = std::fs::read_to_string(&doc_path).unwrap();

    assert!(doc.contains("## Adopted Existing Backends"));
    assert!(doc.contains("handle_passed_token` is empty"));
    assert!(doc.contains("backend_pipe"));
    assert!(doc.contains("HandoffFallbackReason::AdoptedBackend"));
}
