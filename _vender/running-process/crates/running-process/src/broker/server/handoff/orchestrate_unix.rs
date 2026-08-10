//! Production-shaped orchestration of one Unix `SCM_RIGHTS` handle-passing
//! handoff (#354, slice 4).
//!
//! Mirrors the Windows sequence in [`super::orchestrate`] with the stages a
//! Unix handoff actually has. `SCM_RIGHTS` carries the duplicated file
//! descriptor *inside* the message, so there is no separate "duplicate"
//! step followed by a "deliver the handle value" step: one
//! `sendmsg(SCM_RIGHTS)` call both duplicates the descriptor into the
//! backend and tells the backend about it. The broker-side sequence is:
//!
//! 1. send the broker-held connection fd plus the one-time token to the
//!    backend handoff socket ([`super::try_send_scm_rights`]),
//! 2. wait for the backend acknowledgement observed by a
//!    [`UnixHandoffAckWait`] channel, and
//! 3. complete the pending entry in the [`HandoffAckRegistry`], consuming
//!    the one-time token exactly once.
//!
//! Any failure at any step abandons the handoff: the one-time token is
//! revoked, the pending ACK entry is removed, and the caller receives
//! [`UnixHandoffOutcome::FallbackToReconnect`]. The negotiated
//! `backend_pipe` reconnect path stays authoritative; orchestration
//! failures are silent optimization failures, never client errors, and
//! this function never panics on transport, delivery, or registry errors.
//!
//! # Descriptor ownership contract
//!
//! Unlike the Windows `DuplicateHandle` path there is no cross-process
//! leak the broker cannot clean up: the broker keeps ownership of its own
//! `request.fd` at every stage (`SCM_RIGHTS` sends a *duplicate*), so on
//! fallback the caller may simply close the broker-held descriptor. What
//! the broker *cannot* undo is a duplicate that already reached the
//! backend: once the send succeeded, the backend holds its own descriptor
//! until it closes it. [`UnixHandoffFallback::fd_reached_backend`] records
//! that honestly so callers can log it instead of pretending the duplicate
//! was reclaimed.

use std::time::Instant;

use super::ack::HandoffAckRegistry;
use super::fallback::{HandoffFallbackDecision, HandoffFallbackReason};
use super::handoff_token::{HandoffToken, HandoffTokenStore};
use super::orchestrate::HandoffDeliveryError;
use super::unix::{
    try_send_scm_rights, ScmRightsAttempt, ScmRightsResult, ScmRightsSuccess, UnixFileDescriptor,
    UnixHandoffSocket,
};
use super::AcknowledgedHandoff;

/// Inputs for one orchestrated Unix `SCM_RIGHTS` handle-passing handoff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnixHandoffRequest {
    /// Broker-owned connection file descriptor to pass to the backend.
    pub fd: UnixFileDescriptor,
    /// Backend handoff socket that should receive the file descriptor.
    pub backend_socket: UnixHandoffSocket,
    /// One-time token issued at Hello time and registered for an ACK.
    pub token: HandoffToken,
}

impl UnixHandoffRequest {
    /// Build inputs for one orchestrated handoff.
    pub fn new(
        fd: UnixFileDescriptor,
        backend_socket: UnixHandoffSocket,
        token: HandoffToken,
    ) -> Self {
        Self {
            fd,
            backend_socket,
            token,
        }
    }
}

/// Acknowledgement channel observing the backend's adoption of a
/// handed-off connection.
///
/// `SCM_RIGHTS` already delivers the descriptor and token in one message,
/// so unlike the Windows [`super::HandoffDelivery`] trait there is no
/// separate deliver step to abstract — only the ACK wait. Production wire
/// delivery of the ACK does not exist yet (the v1 envelope reserves no
/// backend-to-broker control frame); the orchestration treats the wait as
/// a pluggable step so the sequencing and fallback contract are real
/// today.
pub trait UnixHandoffAckWait {
    /// Block until the backend acknowledges adopting the handed-off
    /// connection, or until `deadline`.
    ///
    /// Returns the instant the acknowledgement was observed. The
    /// orchestrator still validates that instant against the
    /// [`HandoffAckRegistry`] deadline registered at issuance, so an ACK
    /// channel that misjudges the deadline cannot complete an overdue
    /// handoff.
    fn await_backend_ack(
        &mut self,
        token: &HandoffToken,
        deadline: Instant,
    ) -> Result<Instant, HandoffDeliveryError>;
}

/// Step of the orchestration at which a Unix handoff was abandoned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnixHandoffStage {
    /// `sendmsg(SCM_RIGHTS)` to the backend handoff socket failed.
    Send,
    /// The backend acknowledgement was not observed before the deadline.
    AwaitAck,
    /// The ACK registry rejected the acknowledgement (overdue or already
    /// expired by a sweep).
    Acknowledge,
}

/// A handoff completed end to end: descriptor sent with its token and
/// acknowledged before the registry deadline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedUnixHandoff {
    /// Successful `SCM_RIGHTS` send into the backend.
    pub sent: ScmRightsSuccess,
    /// Timely backend acknowledgement that consumed the one-time token.
    pub acknowledged: AcknowledgedHandoff,
}

/// A handoff abandoned at some orchestration stage.
///
/// The one-time token has been revoked and the pending ACK entry removed;
/// the caller must keep using the negotiated `backend_pipe` reconnect path.
/// The broker still owns `broker_fd` and may close it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnixHandoffFallback {
    /// Stage at which the handoff was abandoned.
    pub stage: UnixHandoffStage,
    /// Silent reconnect fallback decision for the client-visible contract.
    pub decision: HandoffFallbackDecision,
    /// Broker-owned connection descriptor. `SCM_RIGHTS` only ever sends a
    /// duplicate, so unlike the Windows leak contract the broker retains
    /// ownership and may close this descriptor now that the handoff is
    /// abandoned.
    pub broker_fd: UnixFileDescriptor,
    /// True when the `SCM_RIGHTS` send already succeeded before the
    /// failure: the backend holds a duplicated descriptor the broker
    /// cannot reclaim; it lives until the backend closes it. The revoked
    /// token guarantees the backend can never *adopt* that connection.
    pub fd_reached_backend: bool,
    /// Human-readable failure detail for logs.
    pub detail: String,
}

/// Outcome of one orchestrated Unix `SCM_RIGHTS` handle-passing handoff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnixHandoffOutcome {
    /// The backend adopted the connection before the ACK deadline.
    Completed(CompletedUnixHandoff),
    /// The handoff was abandoned; the client reconnects via `backend_pipe`.
    FallbackToReconnect(UnixHandoffFallback),
}

impl UnixHandoffOutcome {
    /// Return true when the handoff completed end to end.
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed(_))
    }

    /// Return the fallback details when the handoff was abandoned.
    pub fn fallback(&self) -> Option<&UnixHandoffFallback> {
        match self {
            Self::Completed(_) => None,
            Self::FallbackToReconnect(fallback) => Some(fallback),
        }
    }
}

/// Run one production-shaped Unix handoff with the real
/// `sendmsg(SCM_RIGHTS)` transport.
///
/// The token in `request` must have been issued from `tokens` and
/// registered pending in `acks` (the Hello path does both). On success the
/// token is consumed exactly once; on any failure it is revoked and the
/// outcome degrades to the `backend_pipe` reconnect fallback. On non-Unix
/// targets the transport reports `UnsupportedPlatform` and the outcome is
/// the same non-panicking fallback.
pub fn execute_unix_handoff<W>(
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    request: &UnixHandoffRequest,
    ack_wait: &mut W,
) -> UnixHandoffOutcome
where
    W: UnixHandoffAckWait + ?Sized,
{
    execute_unix_handoff_with_transport(tokens, acks, request, try_send_scm_rights, ack_wait)
}

/// Run one orchestrated Unix handoff with an explicit send transport.
///
/// Platform-neutral tests inject a mock transport here; production callers
/// use [`execute_unix_handoff`].
pub fn execute_unix_handoff_with_transport<T, W>(
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    request: &UnixHandoffRequest,
    transport: T,
    ack_wait: &mut W,
) -> UnixHandoffOutcome
where
    T: FnOnce(&ScmRightsAttempt) -> ScmRightsResult,
    W: UnixHandoffAckWait + ?Sized,
{
    let attempt = ScmRightsAttempt::new(request.fd, request.backend_socket.clone(), request.token);
    let sent = match transport(&attempt) {
        Ok(success) => success,
        Err(error) => {
            acks.abandon(tokens, &request.token);
            let fd_may_have_reached_backend = error.fd_may_have_reached_backend();
            return abandoned(
                UnixHandoffStage::Send,
                error.fallback_decision(),
                request.fd,
                fd_may_have_reached_backend,
                error.to_string(),
            );
        }
    };

    let deadline = ack_deadline_from(acks, Instant::now());
    let acknowledged_at = match ack_wait.await_backend_ack(&request.token, deadline) {
        Ok(at) => at,
        Err(error) => {
            acks.abandon(tokens, &request.token);
            return abandoned(
                UnixHandoffStage::AwaitAck,
                HandoffFallbackDecision::new(HandoffFallbackReason::BackendAckTimeout),
                request.fd,
                true,
                error.to_string(),
            );
        }
    };

    match acks.acknowledge(tokens, &request.token, acknowledged_at) {
        Ok(acknowledged) => {
            UnixHandoffOutcome::Completed(CompletedUnixHandoff { sent, acknowledged })
        }
        Err(error) => {
            // AckDeadlineExceeded already revoked the token; TokenNotPending
            // means a sweep expired it. Revoke defensively either way so no
            // error path can leave the one-time token presentable.
            tokens.revoke(&request.token);
            abandoned(
                UnixHandoffStage::Acknowledge,
                HandoffFallbackDecision::new(HandoffFallbackReason::BackendAckTimeout),
                request.fd,
                true,
                error.to_string(),
            )
        }
    }
}

fn abandoned(
    stage: UnixHandoffStage,
    decision: HandoffFallbackDecision,
    broker_fd: UnixFileDescriptor,
    fd_reached_backend: bool,
    detail: String,
) -> UnixHandoffOutcome {
    UnixHandoffOutcome::FallbackToReconnect(UnixHandoffFallback {
        stage,
        decision,
        broker_fd,
        fd_reached_backend,
        detail,
    })
}

fn ack_deadline_from(acks: &HandoffAckRegistry, now: Instant) -> Instant {
    now.checked_add(acks.ack_deadline()).unwrap_or(now)
}
