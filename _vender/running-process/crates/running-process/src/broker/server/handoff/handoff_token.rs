//! One-time token verification for future broker-to-backend handoff.
//!
//! These tokens authenticate the handoff control message against the client
//! connection that is about to be adopted. The store is intentionally bounded:
//! pending tokens have a maximum count, a TTL, and a collision retry limit.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

/// Number of bytes in one handoff token: 128 bits.
pub const HANDOFF_TOKEN_BYTES: usize = 16;

/// Default maximum number of handoff tokens retained by one broker process.
pub const DEFAULT_MAX_PENDING_HANDOFF_TOKENS: usize = 1024;

/// Default lifetime for a pending handoff token.
pub const DEFAULT_HANDOFF_TOKEN_TTL: Duration = Duration::from_secs(30);

/// Default number of random candidates tried when avoiding in-process
/// collisions.
pub const DEFAULT_HANDOFF_TOKEN_COLLISION_ATTEMPTS: usize = 16;

/// Opaque 128-bit one-time token used to verify a pending handoff.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct HandoffToken([u8; HANDOFF_TOKEN_BYTES]);

impl HandoffToken {
    /// Generate one token from operating-system randomness.
    pub fn generate() -> Result<Self, HandoffTokenError> {
        let mut bytes = [0_u8; HANDOFF_TOKEN_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// Build a token from exact bytes.
    pub fn from_bytes(bytes: [u8; HANDOFF_TOKEN_BYTES]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw token bytes for wire encoding.
    pub fn as_bytes(&self) -> &[u8; HANDOFF_TOKEN_BYTES] {
        &self.0
    }

    /// Return the raw token bytes for wire encoding.
    pub fn into_bytes(self) -> [u8; HANDOFF_TOKEN_BYTES] {
        self.0
    }
}

impl fmt::Debug for HandoffToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HandoffToken(<redacted>)")
    }
}

impl From<[u8; HANDOFF_TOKEN_BYTES]> for HandoffToken {
    fn from(value: [u8; HANDOFF_TOKEN_BYTES]) -> Self {
        Self::from_bytes(value)
    }
}

/// Runtime bounds for pending handoff token verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffTokenStoreConfig {
    /// Maximum pending tokens retained at once.
    pub max_pending_tokens: usize,
    /// Maximum age of an unconsumed token.
    pub token_ttl: Duration,
    /// Maximum random candidates attempted to avoid collisions.
    pub collision_attempts: usize,
}

impl HandoffTokenStoreConfig {
    /// Build a config, clamping zero values to safe non-zero defaults.
    pub fn new(max_pending_tokens: usize, token_ttl: Duration) -> Self {
        Self {
            max_pending_tokens: max_pending_tokens.max(1),
            token_ttl: if token_ttl.is_zero() {
                Duration::from_millis(1)
            } else {
                token_ttl
            },
            collision_attempts: DEFAULT_HANDOFF_TOKEN_COLLISION_ATTEMPTS,
        }
    }

    /// Override the collision retry bound.
    pub fn with_collision_attempts(mut self, collision_attempts: usize) -> Self {
        self.collision_attempts = collision_attempts.max(1);
        self
    }
}

impl Default for HandoffTokenStoreConfig {
    fn default() -> Self {
        Self {
            max_pending_tokens: DEFAULT_MAX_PENDING_HANDOFF_TOKENS,
            token_ttl: DEFAULT_HANDOFF_TOKEN_TTL,
            collision_attempts: DEFAULT_HANDOFF_TOKEN_COLLISION_ATTEMPTS,
        }
    }
}

/// Bounded one-time token store for future handoff verification.
#[derive(Debug)]
pub struct HandoffTokenStore {
    config: HandoffTokenStoreConfig,
    pending: HashMap<HandoffToken, PendingHandoffToken>,
}

impl HandoffTokenStore {
    /// Create an empty store with default bounds.
    pub fn new() -> Self {
        Self::with_config(HandoffTokenStoreConfig::default())
    }

    /// Create an empty store with explicit bounds.
    pub fn with_config(config: HandoffTokenStoreConfig) -> Self {
        Self {
            config,
            pending: HashMap::new(),
        }
    }

    /// Return the number of currently pending, non-pruned tokens.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Issue one pending token from operating-system randomness.
    pub fn issue(&mut self, now: Instant) -> Result<HandoffToken, HandoffTokenError> {
        self.issue_with_random128(now, || {
            let mut bytes = [0_u8; HANDOFF_TOKEN_BYTES];
            getrandom::fill(&mut bytes)?;
            Ok(bytes)
        })
    }

    /// Issue one pending token from a deterministic random source.
    ///
    /// Tests use this to force collisions and capacity pressure without
    /// weakening the production randomness path.
    pub fn issue_with_random128<F>(
        &mut self,
        now: Instant,
        mut next_random128: F,
    ) -> Result<HandoffToken, HandoffTokenError>
    where
        F: FnMut() -> Result<[u8; HANDOFF_TOKEN_BYTES], HandoffTokenError>,
    {
        self.prune_expired(now);
        if self.pending.len() >= self.config.max_pending_tokens {
            return Err(HandoffTokenError::PendingLimitReached {
                max_pending_tokens: self.config.max_pending_tokens,
            });
        }

        for _ in 0..self.config.collision_attempts {
            let token = HandoffToken::from_bytes(next_random128()?);
            if self.pending.contains_key(&token) {
                continue;
            }

            self.pending.insert(
                token,
                PendingHandoffToken {
                    expires_at: expires_at(now, self.config.token_ttl),
                },
            );
            return Ok(token);
        }

        Err(HandoffTokenError::CollisionExhausted {
            attempts: self.config.collision_attempts,
        })
    }

    /// Consume `expected` exactly once if the backend presented the same token.
    ///
    /// A mismatch never consumes either token. Expired tokens are removed before
    /// returning an expiry error so the pending set remains bounded.
    pub fn consume_matching(
        &mut self,
        expected: &HandoffToken,
        presented: &HandoffToken,
        now: Instant,
    ) -> Result<(), HandoffTokenError> {
        self.prune_expired_except(now, Some(expected));

        let Some(pending) = self.pending.get(expected) else {
            return Err(HandoffTokenError::TokenNotPending);
        };
        if now >= pending.expires_at {
            self.pending.remove(expected);
            return Err(HandoffTokenError::TokenExpired);
        }
        if expected != presented {
            return Err(HandoffTokenError::TokenMismatch);
        }

        self.pending.remove(expected);
        Ok(())
    }

    /// Revoke one pending token without consuming it.
    ///
    /// Returns true when the token was pending. The broker uses this when a
    /// handoff is abandoned (backend ACK deadline expired) so a late backend
    /// presentation of the token is rejected as not pending.
    pub fn revoke(&mut self, token: &HandoffToken) -> bool {
        self.pending.remove(token).is_some()
    }

    /// Drop every expired token and return how many entries were removed.
    pub fn prune_expired(&mut self, now: Instant) -> usize {
        self.prune_expired_except(now, None)
    }

    fn prune_expired_except(&mut self, now: Instant, except: Option<&HandoffToken>) -> usize {
        let before = self.pending.len();
        self.pending.retain(|token, pending| {
            except.is_some_and(|expected| expected == token) || now < pending.expires_at
        });
        before - self.pending.len()
    }
}

impl Default for HandoffTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors raised while issuing or consuming handoff tokens.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HandoffTokenError {
    /// Random byte generation failed.
    #[error("handoff token random generation failed: {0}")]
    Random(String),
    /// The pending token set is full.
    #[error("handoff token pending limit reached ({max_pending_tokens})")]
    PendingLimitReached {
        /// Maximum pending tokens allowed.
        max_pending_tokens: usize,
    },
    /// All random candidates collided with existing pending tokens.
    #[error("handoff token allocation exhausted after {attempts} collision attempts")]
    CollisionExhausted {
        /// Number of random candidates attempted.
        attempts: usize,
    },
    /// The backend presented a token that does not match the expected handoff.
    #[error("handoff token mismatch")]
    TokenMismatch,
    /// The expected token existed but exceeded its TTL.
    #[error("handoff token expired")]
    TokenExpired,
    /// The expected token is unknown or has already been consumed.
    #[error("handoff token is not pending")]
    TokenNotPending,
}

impl From<getrandom::Error> for HandoffTokenError {
    fn from(value: getrandom::Error) -> Self {
        Self::Random(value.to_string())
    }
}

#[derive(Clone, Debug)]
struct PendingHandoffToken {
    expires_at: Instant,
}

fn expires_at(now: Instant, ttl: Duration) -> Instant {
    now.checked_add(ttl).unwrap_or(now)
}
