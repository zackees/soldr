#![cfg(feature = "client")]

use std::path::Path;

use running_process::broker::server::handoff::{
    HandoffFallbackDecision, HandoffFallbackReason, DUPLICATE_HANDLE_TRANSPORT_SUPPORTED,
    SCM_RIGHTS_TRANSPORT_SUPPORTED,
};

fn repo_doc() -> String {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let doc_path = repo_root.join("docs/v1-handoff-optimization.md");
    std::fs::read_to_string(&doc_path).unwrap()
}

fn all_fallback_reasons() -> [HandoffFallbackReason; 8] {
    [
        HandoffFallbackReason::ClientUnsupported,
        HandoffFallbackReason::ServicePolicyDisabled,
        HandoffFallbackReason::FdPressureDisabled,
        HandoffFallbackReason::FailedAttemptRateLimited,
        HandoffFallbackReason::PermissionDenied,
        HandoffFallbackReason::IntegrityMismatch,
        HandoffFallbackReason::BackendAckTimeout,
        HandoffFallbackReason::AdoptedBackend,
    ]
}

#[test]
fn platform_transport_support_matches_target_family() {
    assert_eq!(DUPLICATE_HANDLE_TRANSPORT_SUPPORTED, cfg!(windows));
    assert_eq!(SCM_RIGHTS_TRANSPORT_SUPPORTED, cfg!(unix));
}

#[test]
fn every_handoff_fallback_reason_stays_silent_reconnect() {
    for reason in all_fallback_reasons() {
        let fallback = HandoffFallbackDecision::new(reason);

        assert!(fallback.uses_backend_reconnect(), "{reason:?}");
        assert!(!fallback.sends_client_error(), "{reason:?}");
    }
}

#[test]
fn docs_pin_cross_os_handoff_acceptance_evidence() {
    let doc = repo_doc();

    assert!(doc.contains("## Cross-OS Acceptance Evidence"));
    assert!(doc.contains("| Windows | `DuplicateHandle`"));
    assert!(doc.contains("| Linux | `SCM_RIGHTS`"));
    assert!(doc.contains("| macOS | `SCM_RIGHTS`"));
    assert!(doc.contains("soldr cargo test -p running-process --test broker --features client handoff -- --nocapture"));
    assert!(doc.contains("DUPLICATE_HANDLE_TRANSPORT_SUPPORTED"));
    assert!(doc.contains("SCM_RIGHTS_TRANSPORT_SUPPORTED"));
}
