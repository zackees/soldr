//! Handoff scaffolding for the optional Phase 6 handle-passing path.
//!
//! Platform-specific handle transfer is intentionally not wired here. This
//! module owns the reusable verification pieces that both `DuplicateHandle`
//! and `SCM_RIGHTS` paths will need before a handed-off connection is trusted.

pub mod ack;
pub mod fallback;
pub mod handoff_token;
pub mod latency;
pub mod orchestrate;
pub mod orchestrate_unix;
pub mod pending;
pub mod unix;
pub mod windows;
pub mod wire;

pub use ack::{
    AcknowledgedHandoff, ExpiredHandoff, HandoffAckError, HandoffAckRegistry,
    PendingHandoffBackend, DEFAULT_HANDOFF_ACK_DEADLINE,
};
pub use fallback::{
    HandoffAttemptDecision, HandoffAttemptFailure, HandoffAttemptInputs, HandoffFallbackDecision,
    HandoffFallbackPolicy, HandoffFallbackReason, HandoffFallbackState,
    DEFAULT_HANDOFF_FAILED_ATTEMPTS_PER_WINDOW, DEFAULT_HANDOFF_FAILED_ATTEMPT_WINDOW,
};
pub use handoff_token::{
    HandoffToken, HandoffTokenError, HandoffTokenStore, HandoffTokenStoreConfig,
    DEFAULT_HANDOFF_TOKEN_COLLISION_ATTEMPTS, DEFAULT_HANDOFF_TOKEN_TTL,
    DEFAULT_MAX_PENDING_HANDOFF_TOKENS, HANDOFF_TOKEN_BYTES,
};
pub use latency::{
    collect_latency_samples, compare_handoff_latency, summarize_latency_samples,
    HandoffLatencyComparison, HandoffLatencyError, HandoffLatencySummary,
};
#[cfg(windows)]
pub use orchestrate::execute_verified_windows_handoff;
pub use orchestrate::{
    execute_windows_handoff, execute_windows_handoff_with_transport, CompletedWindowsHandoff,
    HandoffDelivery, HandoffDeliveryError, WindowsHandoffFallback, WindowsHandoffOutcome,
    WindowsHandoffRequest, WindowsHandoffStage,
};
pub use orchestrate_unix::{
    execute_unix_handoff, execute_unix_handoff_with_transport, CompletedUnixHandoff,
    UnixHandoffAckWait, UnixHandoffFallback, UnixHandoffOutcome, UnixHandoffRequest,
    UnixHandoffStage,
};
pub use pending::{
    PendingHandoffOverflow, PendingHandoffQueue, PendingHandoffQueueConfig,
    DEFAULT_MAX_PENDING_HANDOFFS, DEFAULT_PENDING_HANDOFF_TTL,
};
#[cfg(unix)]
pub use unix::try_send_scm_rights_over;
pub use unix::{
    try_send_scm_rights, ScmRightsAttempt, ScmRightsError, ScmRightsResult, ScmRightsSuccess,
    UnixFileDescriptor, UnixHandoffSocket, SCM_RIGHTS_TRANSPORT_SUPPORTED,
};
pub use windows::{
    try_duplicate_handle, DuplicateHandleAttempt, DuplicateHandleError, DuplicateHandleResult,
    DuplicateHandleSuccess, WindowsHandleValue, DUPLICATE_HANDLE_TRANSPORT_SUPPORTED,
};
pub use wire::{
    handoff_ack_frame, handoff_offer_frame, handoff_ready_frame, validate_handoff_frame,
    WireHandoffDelivery, HANDOFF_PAYLOAD_PROTOCOL,
};
