//! Backend ACK deadline tracking for pending handoffs (#354, slice 2).
//!
//! Issuing a `Negotiated.handle_passed_token` is not enough to consider a
//! handoff complete: the backend must report that it adopted the passed
//! handle before a broker-side deadline. This module owns that contract.
//!
//! Transport note: since #354 slice 6 the v1 envelope carries a
//! backend-to-broker `HandoffAck` frame (see [`super::wire`]); the broker
//! observes it through
//! [`WireHandoffDelivery`](super::wire::WireHandoffDelivery) and reports it
//! here through [`HandoffAckRegistry::acknowledge`]. The backend acceptance
//! path (`backend_lib::accept_handed_off`) consumes the one-time token
//! before the ACK is sent. Until an ACK arrives within the deadline,
//! `Negotiated.backend_pipe` reconnect remains the correctness path; an
//! expired handoff is abandoned and falls back to reconnect.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::fallback::{HandoffAttemptFailure, HandoffFallbackDecision, HandoffFallbackReason};
use super::handoff_token::{HandoffToken, HandoffTokenStore};

/// Default deadline for the backend to acknowledge a passed handle.
///
/// Deliberately much shorter than [`super::DEFAULT_HANDOFF_TOKEN_TTL`]
/// (30s): a backend that has actually received a handle acknowledges in
/// milliseconds, so waiting longer only delays the reconnect fallback.
pub const DEFAULT_HANDOFF_ACK_DEADLINE: Duration = Duration::from_secs(5);

/// Identity of the backend expected to acknowledge one pending handoff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingHandoffBackend {
    /// Service name the handoff was negotiated for.
    pub service_name: String,
    /// Backend process ID when known; `0` when the broker only knows the
    /// service (the Hello path issues tokens before resolving a live pid).
    pub backend_pid: u32,
}

impl PendingHandoffBackend {
    /// Identity for a backend known only by its service name.
    pub fn for_service(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            backend_pid: 0,
        }
    }

    /// Identity for a backend known by service name and process ID.
    pub fn new(service_name: impl Into<String>, backend_pid: u32) -> Self {
        Self {
            service_name: service_name.into(),
            backend_pid,
        }
    }
}

/// Broker-side registry of handoffs awaiting backend acknowledgement.
///
/// Keyed by the issued one-time token. Entries leave the registry exactly
/// once: through [`acknowledge`](Self::acknowledge) (completed) or through
/// deadline expiry (failed, token revoked, reconnect fallback).
#[derive(Debug)]
pub struct HandoffAckRegistry {
    ack_deadline: Duration,
    pending: HashMap<HandoffToken, PendingAckEntry>,
}

impl HandoffAckRegistry {
    /// Create an empty registry with the default ACK deadline.
    pub fn new() -> Self {
        Self::with_ack_deadline(DEFAULT_HANDOFF_ACK_DEADLINE)
    }

    /// Create an empty registry with an explicit ACK deadline.
    ///
    /// A zero deadline is clamped to one millisecond so every registered
    /// handoff gets a non-empty acknowledgement window.
    pub fn with_ack_deadline(ack_deadline: Duration) -> Self {
        Self {
            ack_deadline: if ack_deadline.is_zero() {
                Duration::from_millis(1)
            } else {
                ack_deadline
            },
            pending: HashMap::new(),
        }
    }

    /// Return the configured ACK deadline.
    pub fn ack_deadline(&self) -> Duration {
        self.ack_deadline
    }

    /// Return the number of handoffs still awaiting acknowledgement.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Return the backend identity recorded for a pending handoff.
    pub fn pending_backend(&self, token: &HandoffToken) -> Option<&PendingHandoffBackend> {
        self.pending.get(token).map(|entry| &entry.backend)
    }

    /// Register one issued token as awaiting backend acknowledgement.
    ///
    /// Re-registering the same token replaces the previous entry; the token
    /// store already guarantees issued tokens are unique while pending.
    pub fn register(&mut self, token: HandoffToken, backend: PendingHandoffBackend, now: Instant) {
        let ack_deadline_at = now.checked_add(self.ack_deadline).unwrap_or(now);
        self.pending.insert(
            token,
            PendingAckEntry {
                backend,
                issued_at: now,
                ack_deadline_at,
            },
        );
    }

    /// Record that the backend adopted the handed-off connection.
    ///
    /// On success the pending entry transitions to completed (removed) and
    /// the one-time token is revoked from `tokens` so it can never be
    /// presented again. A second ACK for the same token, an ACK for an
    /// unknown token, or an ACK after expiry sweep is rejected with
    /// [`HandoffAckError::TokenNotPending`]. An ACK past the deadline (when
    /// no sweep ran yet) is rejected with
    /// [`HandoffAckError::AckDeadlineExceeded`] and the token is revoked.
    pub fn acknowledge(
        &mut self,
        tokens: &mut HandoffTokenStore,
        token: &HandoffToken,
        now: Instant,
    ) -> Result<AcknowledgedHandoff, HandoffAckError> {
        let Some(entry) = self.pending.remove(token) else {
            return Err(HandoffAckError::TokenNotPending);
        };
        if now >= entry.ack_deadline_at {
            tokens.revoke(token);
            return Err(HandoffAckError::AckDeadlineExceeded {
                backend: entry.backend,
                deadline: self.ack_deadline,
            });
        }

        tokens.revoke(token);
        Ok(AcknowledgedHandoff {
            token: *token,
            backend: entry.backend,
            waited: now.saturating_duration_since(entry.issued_at),
        })
    }

    /// Abandon one pending handoff before its ACK deadline.
    ///
    /// Used when a broker-side step (handle duplication or delivery) fails
    /// after issuance: the pending entry is removed and the one-time token
    /// is revoked from `tokens` even when the entry was already gone, so a
    /// late backend presentation of the token is always rejected. Returns
    /// the backend identity when an entry was pending.
    pub fn abandon(
        &mut self,
        tokens: &mut HandoffTokenStore,
        token: &HandoffToken,
    ) -> Option<PendingHandoffBackend> {
        let entry = self.pending.remove(token);
        tokens.revoke(token);
        entry.map(|entry| entry.backend)
    }

    /// Expire every pending handoff whose ACK deadline has passed.
    ///
    /// Each expired handoff has its one-time token revoked from `tokens`
    /// (so a late backend ACK or token presentation is rejected) and is
    /// returned to the caller, which must fall back to `backend_pipe`
    /// reconnect for the affected connection.
    pub fn expire_overdue(
        &mut self,
        tokens: &mut HandoffTokenStore,
        now: Instant,
    ) -> Vec<ExpiredHandoff> {
        let overdue: Vec<HandoffToken> = self
            .pending
            .iter()
            .filter(|(_, entry)| now >= entry.ack_deadline_at)
            .map(|(token, _)| *token)
            .collect();

        let mut expired = Vec::with_capacity(overdue.len());
        for token in overdue {
            let Some(entry) = self.pending.remove(&token) else {
                continue;
            };
            tokens.revoke(&token);
            expired.push(ExpiredHandoff {
                token,
                backend: entry.backend,
                deadline: self.ack_deadline,
            });
        }
        expired
    }
}

impl Default for HandoffAckRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A handoff completed by a timely backend acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcknowledgedHandoff {
    /// The consumed one-time token.
    pub token: HandoffToken,
    /// Backend that acknowledged the handoff.
    pub backend: PendingHandoffBackend,
    /// Time the broker waited between issuance and acknowledgement.
    pub waited: Duration,
}

/// A pending handoff abandoned because the backend never acknowledged it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredHandoff {
    /// The revoked one-time token.
    pub token: HandoffToken,
    /// Backend that failed to acknowledge in time.
    pub backend: PendingHandoffBackend,
    /// Deadline that was exceeded.
    pub deadline: Duration,
}

impl ExpiredHandoff {
    /// Map this expiry onto the shared handoff failure classification.
    pub fn attempt_failure(&self) -> HandoffAttemptFailure {
        HandoffAttemptFailure::BackendAckTimeout
    }

    /// Return the silent reconnect fallback decision for this expiry.
    pub fn fallback_decision(&self) -> HandoffFallbackDecision {
        HandoffFallbackDecision::new(HandoffFallbackReason::BackendAckTimeout)
    }
}

/// Errors raised when a backend acknowledgement cannot complete a handoff.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HandoffAckError {
    /// The token is unknown, already acknowledged, or already expired.
    #[error("handoff ACK token is not pending")]
    TokenNotPending,
    /// The acknowledgement arrived after the broker ACK deadline.
    #[error("backend ACK deadline ({deadline:?}) exceeded for {backend:?}")]
    AckDeadlineExceeded {
        /// Backend that acknowledged too late.
        backend: PendingHandoffBackend,
        /// Deadline that was exceeded.
        deadline: Duration,
    },
}

impl HandoffAckError {
    /// Map this error onto the shared handoff failure classification, when
    /// it represents a backend ACK timeout.
    pub fn attempt_failure(&self) -> Option<HandoffAttemptFailure> {
        match self {
            Self::TokenNotPending => None,
            Self::AckDeadlineExceeded { .. } => Some(HandoffAttemptFailure::BackendAckTimeout),
        }
    }
}

#[derive(Clone, Debug)]
struct PendingAckEntry {
    backend: PendingHandoffBackend,
    issued_at: Instant,
    ack_deadline_at: Instant,
}
