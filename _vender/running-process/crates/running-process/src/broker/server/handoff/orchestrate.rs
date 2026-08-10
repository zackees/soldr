//! Production-shaped orchestration of one Windows handle-passing handoff
//! (#354, slice 3).
//!
//! This module composes the pieces landed by earlier slices into one
//! broker-side sequence:
//!
//! 1. duplicate the broker-held client pipe into the verified backend
//!    process ([`super::try_duplicate_handle`], or the verified
//!    [`BackendHandle`](crate::broker::backend_handle::BackendHandle)
//!    bridge),
//! 2. deliver the duplicated handle value plus the one-time token to the
//!    backend through a [`HandoffDelivery`] implementation,
//! 3. wait for the backend acknowledgement observed by the delivery
//!    channel, and
//! 4. complete the pending entry in the [`HandoffAckRegistry`], consuming
//!    the one-time token exactly once.
//!
//! Any failure at any step abandons the handoff: the one-time token is
//! revoked, the pending ACK entry is removed, and the caller receives
//! [`WindowsHandoffOutcome::FallbackToReconnect`]. The negotiated
//! `backend_pipe` reconnect path stays authoritative; orchestration
//! failures are silent optimization failures, never client errors, and
//! this function never panics on transport, delivery, or registry errors.
//!
//! # Delivery mechanism
//!
//! Delivery of the `(handle value, token)` pair is abstracted behind the
//! [`HandoffDelivery`] trait so the orchestration sequence, token
//! lifecycle, and fallback contract stay transport-agnostic. The
//! production implementation is
//! [`WireHandoffDelivery`](super::wire::WireHandoffDelivery) (#354 slice
//! 6), which sends a `HandoffOffer` frame over a framed broker↔backend
//! control connection and waits for the matching `HandoffAck`; tests also
//! deliver over the child-helper stdin/stdout protocol from the #358/#363
//! smoke tests.
//!
//! # Handle leak contract
//!
//! `DuplicateHandle` places the duplicated handle directly into the
//! *backend's* handle table. Once duplication has succeeded, the broker
//! cannot close that handle: closing a handle owned by another process
//! would require a second `DUPLICATE_CLOSE_SOURCE` round-trip that is not
//! part of this slice. If delivery or acknowledgement fails after
//! duplication, the duplicated handle therefore leaks in the backend
//! process until the backend exits. The outcome records it in
//! [`WindowsHandoffFallback::leaked_backend_handle`] so callers can log
//! and monitor the leak honestly instead of pretending cleanup happened.

use std::time::Instant;

use super::ack::{HandoffAckRegistry, PendingHandoffBackend};
use super::fallback::{HandoffFallbackDecision, HandoffFallbackReason};
use super::handoff_token::{HandoffToken, HandoffTokenStore};
use super::windows::{
    try_duplicate_handle, DuplicateHandleAttempt, DuplicateHandleResult, DuplicateHandleSuccess,
    WindowsHandleValue,
};
use super::AcknowledgedHandoff;

/// Inputs for one orchestrated Windows handle-passing handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowsHandoffRequest {
    /// Broker-owned pipe handle to duplicate into the backend.
    pub pipe_handle: WindowsHandleValue,
    /// Verified backend process ID receiving the duplicated handle.
    pub backend_pid: u32,
    /// One-time token issued at Hello time and registered for an ACK.
    pub token: HandoffToken,
}

impl WindowsHandoffRequest {
    /// Build inputs for one orchestrated handoff.
    pub fn new(pipe_handle: WindowsHandleValue, backend_pid: u32, token: HandoffToken) -> Self {
        Self {
            pipe_handle,
            backend_pid,
            token,
        }
    }
}

/// Delivery channel carrying the duplicated handle value and one-time token
/// from the broker to the backend process.
///
/// Production wire delivery does not exist yet (see the module docs); the
/// orchestration treats delivery as a pluggable step so the sequencing and
/// fallback contract are real today.
pub trait HandoffDelivery {
    /// Deliver the duplicated handle value and paired token to the backend.
    ///
    /// The handle value is only meaningful inside the backend's handle
    /// table. A returned error means the backend cannot be assumed to know
    /// about the handle; the orchestrator abandons the handoff.
    fn deliver(
        &mut self,
        handle: WindowsHandleValue,
        token: &HandoffToken,
    ) -> Result<(), HandoffDeliveryError>;

    /// Block until the backend acknowledges adopting the handed-off
    /// connection, or until `deadline`.
    ///
    /// Returns the instant the acknowledgement was observed. The
    /// orchestrator still validates that instant against the
    /// [`HandoffAckRegistry`] deadline registered at issuance, so a
    /// delivery channel that misjudges the deadline cannot complete an
    /// overdue handoff.
    fn await_backend_ack(
        &mut self,
        token: &HandoffToken,
        deadline: Instant,
    ) -> Result<Instant, HandoffDeliveryError>;
}

/// Errors surfaced by a [`HandoffDelivery`] channel.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HandoffDeliveryError {
    /// The handle value and token could not be delivered to the backend.
    #[error("handoff delivery to backend failed: {detail}")]
    DeliveryFailed {
        /// Human-readable failure detail for logs.
        detail: String,
    },
    /// The backend acknowledgement was not observed before the deadline.
    #[error("backend handoff ACK was not observed: {detail}")]
    AckNotObserved {
        /// Human-readable failure detail for logs.
        detail: String,
    },
}

/// Step of the orchestration at which a handoff was abandoned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsHandoffStage {
    /// `DuplicateHandle` into the backend process failed.
    Duplicate,
    /// Delivering the handle value and token to the backend failed.
    Deliver,
    /// The backend acknowledgement was not observed before the deadline.
    AwaitAck,
    /// The ACK registry rejected the acknowledgement (overdue or already
    /// expired by a sweep).
    Acknowledge,
}

/// A handoff completed end to end: handle duplicated, delivered, and
/// acknowledged before the registry deadline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedWindowsHandoff {
    /// Successful duplication into the backend handle table.
    pub duplicated: DuplicateHandleSuccess,
    /// Timely backend acknowledgement that consumed the one-time token.
    pub acknowledged: AcknowledgedHandoff,
}

/// A handoff abandoned at some orchestration stage.
///
/// The one-time token has been revoked and the pending ACK entry removed;
/// the caller must keep using the negotiated `backend_pipe` reconnect path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsHandoffFallback {
    /// Stage at which the handoff was abandoned.
    pub stage: WindowsHandoffStage,
    /// Silent reconnect fallback decision for the client-visible contract.
    pub decision: HandoffFallbackDecision,
    /// Handle already duplicated into the backend's handle table when the
    /// failure occurred. The broker cannot close another process's handle
    /// (see the module-level leak contract); it leaks in the backend until
    /// that process exits. `None` when duplication itself failed.
    pub leaked_backend_handle: Option<WindowsHandleValue>,
    /// Human-readable failure detail for logs.
    pub detail: String,
}

/// Outcome of one orchestrated Windows handle-passing handoff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowsHandoffOutcome {
    /// The backend adopted the connection before the ACK deadline.
    Completed(CompletedWindowsHandoff),
    /// The handoff was abandoned; the client reconnects via `backend_pipe`.
    FallbackToReconnect(WindowsHandoffFallback),
}

impl WindowsHandoffOutcome {
    /// Return true when the handoff completed end to end.
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed(_))
    }

    /// Return the fallback details when the handoff was abandoned.
    pub fn fallback(&self) -> Option<&WindowsHandoffFallback> {
        match self {
            Self::Completed(_) => None,
            Self::FallbackToReconnect(fallback) => Some(fallback),
        }
    }
}

/// Run one production-shaped Windows handoff with the real
/// `DuplicateHandle` transport.
///
/// The token in `request` must have been issued from `tokens` and
/// registered pending in `acks` (the Hello path does both). On success the
/// token is consumed exactly once; on any failure it is revoked and the
/// outcome degrades to the `backend_pipe` reconnect fallback.
pub fn execute_windows_handoff<D>(
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    request: &WindowsHandoffRequest,
    delivery: &mut D,
) -> WindowsHandoffOutcome
where
    D: HandoffDelivery + ?Sized,
{
    execute_windows_handoff_with_transport(tokens, acks, request, try_duplicate_handle, delivery)
}

/// Run one production-shaped Windows handoff for a verified backend.
///
/// Composes the [`BackendHandle`](crate::broker::backend_handle::BackendHandle)
/// identity bridge from #363 with the orchestration sequence: the backend
/// pid comes from the verified daemon identity, and duplication goes
/// through
/// [`BackendHandle::try_duplicate_windows_handoff_handle`](crate::broker::backend_handle::BackendHandle::try_duplicate_windows_handoff_handle).
#[cfg(windows)]
pub fn execute_verified_windows_handoff<D>(
    backend: &crate::broker::backend_handle::BackendHandle,
    pipe_handle: WindowsHandleValue,
    token: HandoffToken,
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    delivery: &mut D,
) -> WindowsHandoffOutcome
where
    D: HandoffDelivery + ?Sized,
{
    let request = WindowsHandoffRequest::new(pipe_handle, backend.daemon_process.pid, token);
    execute_windows_handoff_with_transport(
        tokens,
        acks,
        &request,
        |attempt| {
            backend.try_duplicate_windows_handoff_handle(attempt.pipe_handle, attempt.handoff_token)
        },
        delivery,
    )
}

/// Run one orchestrated handoff with an explicit duplication transport.
///
/// Platform-neutral tests inject a mock transport here; production callers
/// use [`execute_windows_handoff`] or the Windows-only
/// `execute_verified_windows_handoff`.
pub fn execute_windows_handoff_with_transport<T, D>(
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    request: &WindowsHandoffRequest,
    transport: T,
    delivery: &mut D,
) -> WindowsHandoffOutcome
where
    T: FnOnce(&DuplicateHandleAttempt) -> DuplicateHandleResult,
    D: HandoffDelivery + ?Sized,
{
    let attempt =
        DuplicateHandleAttempt::new(request.pipe_handle, request.backend_pid, request.token);
    let duplicated = match transport(&attempt) {
        Ok(success) => success,
        Err(error) => {
            abandon_pending(acks, tokens, &request.token);
            return abandoned(
                WindowsHandoffStage::Duplicate,
                error.fallback_decision(),
                None,
                error.to_string(),
            );
        }
    };
    let backend_handle = duplicated.duplicated_handle;

    if let Err(error) = delivery.deliver(backend_handle, &request.token) {
        abandon_pending(acks, tokens, &request.token);
        return abandoned(
            WindowsHandoffStage::Deliver,
            // The backend never adopts the connection, so the client-visible
            // classification is the same as a missing acknowledgement.
            HandoffFallbackDecision::new(HandoffFallbackReason::BackendAckTimeout),
            Some(backend_handle),
            error.to_string(),
        );
    }

    let deadline = ack_deadline_from(acks, Instant::now());
    let acknowledged_at = match delivery.await_backend_ack(&request.token, deadline) {
        Ok(at) => at,
        Err(error) => {
            abandon_pending(acks, tokens, &request.token);
            return abandoned(
                WindowsHandoffStage::AwaitAck,
                HandoffFallbackDecision::new(HandoffFallbackReason::BackendAckTimeout),
                Some(backend_handle),
                error.to_string(),
            );
        }
    };

    match acks.acknowledge(tokens, &request.token, acknowledged_at) {
        Ok(acknowledged) => WindowsHandoffOutcome::Completed(CompletedWindowsHandoff {
            duplicated,
            acknowledged,
        }),
        Err(error) => {
            // AckDeadlineExceeded already revoked the token; TokenNotPending
            // means a sweep expired it. Revoke defensively either way so no
            // error path can leave the one-time token presentable.
            tokens.revoke(&request.token);
            abandoned(
                WindowsHandoffStage::Acknowledge,
                HandoffFallbackDecision::new(HandoffFallbackReason::BackendAckTimeout),
                Some(backend_handle),
                error.to_string(),
            )
        }
    }
}

/// Abandon one pending handoff: drop the pending ACK entry and revoke the
/// one-time token so a late backend presentation is rejected.
fn abandon_pending(
    acks: &mut HandoffAckRegistry,
    tokens: &mut HandoffTokenStore,
    token: &HandoffToken,
) -> Option<PendingHandoffBackend> {
    acks.abandon(tokens, token)
}

fn abandoned(
    stage: WindowsHandoffStage,
    decision: HandoffFallbackDecision,
    leaked_backend_handle: Option<WindowsHandleValue>,
    detail: String,
) -> WindowsHandoffOutcome {
    WindowsHandoffOutcome::FallbackToReconnect(WindowsHandoffFallback {
        stage,
        decision,
        leaked_backend_handle,
        detail,
    })
}

fn ack_deadline_from(acks: &HandoffAckRegistry, now: Instant) -> Instant {
    now.checked_add(acks.ack_deadline()).unwrap_or(now)
}
