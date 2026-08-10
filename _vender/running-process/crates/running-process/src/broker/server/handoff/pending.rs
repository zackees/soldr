//! Capacity-bounded pending handoff backlog.
//!
//! The real platform transports may need to park accepted client handles while
//! a backend handoff socket or acknowledgement catches up. This model keeps
//! that backlog finite and maps overload into the existing reconnect fallback.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::{HandoffAttemptDecision, HandoffFallbackDecision, HandoffFallbackReason};

/// Default number of pending handoffs retained by one broker process.
pub const DEFAULT_MAX_PENDING_HANDOFFS: usize = 64;

/// Default maximum age for a pending handoff before it is expired.
pub const DEFAULT_PENDING_HANDOFF_TTL: Duration = Duration::from_millis(100);

/// Runtime bounds for the pending handoff backlog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingHandoffQueueConfig {
    /// Maximum handoff attempts allowed to wait for backend handoff progress.
    pub max_pending_handoffs: usize,
    /// Maximum age of an unprocessed pending handoff.
    pub pending_ttl: Duration,
}

impl PendingHandoffQueueConfig {
    /// Build a config, clamping zero values to safe non-zero defaults.
    pub fn new(max_pending_handoffs: usize, pending_ttl: Duration) -> Self {
        Self {
            max_pending_handoffs: max_pending_handoffs.max(1),
            pending_ttl: if pending_ttl.is_zero() {
                Duration::from_millis(1)
            } else {
                pending_ttl
            },
        }
    }
}

impl Default for PendingHandoffQueueConfig {
    fn default() -> Self {
        Self {
            max_pending_handoffs: DEFAULT_MAX_PENDING_HANDOFFS,
            pending_ttl: DEFAULT_PENDING_HANDOFF_TTL,
        }
    }
}

/// FIFO queue for pending handoffs that cannot grow past its configured bound.
#[derive(Debug)]
pub struct PendingHandoffQueue<T> {
    config: PendingHandoffQueueConfig,
    queue: VecDeque<PendingHandoffEntry<T>>,
}

impl<T> PendingHandoffQueue<T> {
    /// Create an empty queue with default bounds.
    pub fn new() -> Self {
        Self::with_config(PendingHandoffQueueConfig::default())
    }

    /// Create an empty queue with explicit bounds.
    pub fn with_config(config: PendingHandoffQueueConfig) -> Self {
        Self {
            config,
            queue: VecDeque::with_capacity(config.max_pending_handoffs),
        }
    }

    /// Return the active queue bounds.
    pub fn config(&self) -> PendingHandoffQueueConfig {
        self.config
    }

    /// Return the number of currently pending, non-pruned handoffs.
    pub fn pending_len(&self) -> usize {
        self.queue.len()
    }

    /// Return true when no handoffs are pending.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Enqueue one handoff in FIFO order.
    ///
    /// Expired entries are pruned first. If the backlog is still full, the
    /// caller must use the returned overflow decision to fall back to reconnect.
    pub fn enqueue(&mut self, handoff: T, now: Instant) -> Result<(), PendingHandoffOverflow> {
        self.expire(now);
        if self.queue.len() >= self.config.max_pending_handoffs {
            return Err(PendingHandoffOverflow {
                max_pending_handoffs: self.config.max_pending_handoffs,
            });
        }

        self.queue.push_back(PendingHandoffEntry {
            handoff,
            expires_at: expires_at(now, self.config.pending_ttl),
        });
        Ok(())
    }

    /// Dequeue the oldest non-expired handoff.
    pub fn dequeue(&mut self, now: Instant) -> Option<T> {
        self.expire(now);
        self.queue.pop_front().map(|entry| entry.handoff)
    }

    /// Drop all expired pending handoffs and return the number removed.
    pub fn expire(&mut self, now: Instant) -> usize {
        let before = self.queue.len();
        self.queue.retain(|entry| now < entry.expires_at);
        before - self.queue.len()
    }
}

impl<T> Default for PendingHandoffQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Overflow raised when the pending handoff queue reaches its configured cap.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("pending handoff queue full ({max_pending_handoffs})")]
pub struct PendingHandoffOverflow {
    /// Maximum pending handoffs allowed before reconnect fallback is required.
    pub max_pending_handoffs: usize,
}

impl PendingHandoffOverflow {
    /// Return the existing fallback reason used for handoff pressure.
    pub fn fallback_reason(&self) -> HandoffFallbackReason {
        HandoffFallbackReason::FdPressureDisabled
    }

    /// Return the silent reconnect fallback for this overload condition.
    pub fn fallback_decision(&self) -> HandoffFallbackDecision {
        HandoffFallbackDecision::new(self.fallback_reason())
    }

    /// Return the full attempt decision for callers that operate on broker decisions.
    pub fn fallback_attempt_decision(&self) -> HandoffAttemptDecision {
        HandoffAttemptDecision::FallbackToReconnect(self.fallback_decision())
    }

    /// Return true when overflow is safe to hide behind reconnect fallback.
    pub fn is_fallback_safe(&self) -> bool {
        let fallback = self.fallback_decision();
        fallback.uses_backend_reconnect() && !fallback.sends_client_error()
    }
}

#[derive(Debug)]
struct PendingHandoffEntry<T> {
    handoff: T,
    expires_at: Instant,
}

fn expires_at(now: Instant, ttl: Duration) -> Instant {
    now.checked_add(ttl).unwrap_or(now)
}
