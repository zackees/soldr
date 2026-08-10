//! Acceptance classifier for backend-side handed-off connections.
//!
//! The platform modules that eventually receive duplicated handles or passed
//! file descriptors can wrap those transport values in [`HandedOffPayload`].
//! This helper validates the one-time handoff token and returns a typed
//! accepted/rejected classification without knowing anything about the
//! underlying transport.

use std::time::Instant;

use crate::broker::server::{
    HandoffToken, HandoffTokenError, HandoffTokenStore, HANDOFF_TOKEN_BYTES,
};

/// Platform-neutral payload delivered to a backend accept loop.
///
/// `connection` is intentionally generic: future Windows and Unix modules can
/// store a duplicated handle, an owned file descriptor, or a test double here
/// without changing the token verification logic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandedOffPayload<T> {
    /// Token the backend expected for this pending handoff.
    pub expected_token: HandoffToken,
    /// Token bytes presented with the platform-specific handoff message.
    pub presented_token: Vec<u8>,
    /// Platform-specific connection payload.
    pub connection: T,
}

impl<T> HandedOffPayload<T> {
    /// Build a payload from the expected token, raw presented token bytes, and
    /// platform-specific connection payload.
    pub fn new(
        expected_token: HandoffToken,
        presented_token: impl Into<Vec<u8>>,
        connection: T,
    ) -> Self {
        Self {
            expected_token,
            presented_token: presented_token.into(),
            connection,
        }
    }

    /// Return the presented token bytes.
    pub fn presented_token(&self) -> &[u8] {
        &self.presented_token
    }

    /// Split the payload into its platform-specific connection value.
    pub fn into_connection(self) -> T {
        self.connection
    }
}

/// A handed-off payload accepted by the backend helper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedHandoff<T> {
    /// Token that was consumed exactly once.
    pub token: HandoffToken,
    /// Platform-specific connection payload.
    pub connection: T,
}

/// A handed-off payload rejected by the backend helper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedHandoff<T> {
    /// Original platform-neutral payload. The caller decides how to drop or
    /// close the platform-specific connection.
    pub payload: HandedOffPayload<T>,
    /// Reason the payload was not accepted.
    pub reason: HandoffRejectionReason,
}

/// Classification returned after backend-side handoff acceptance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandoffAcceptance<T> {
    /// Token validation succeeded and the one-time token was consumed.
    Accepted(AcceptedHandoff<T>),
    /// Token validation failed; the connection payload should not be adopted.
    Rejected(RejectedHandoff<T>),
}

impl<T> HandoffAcceptance<T> {
    /// Return true when the payload was accepted.
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }

    /// Return true when the payload was rejected.
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected(_))
    }

    /// Convert the classification into a `Result`.
    pub fn into_result(self) -> Result<AcceptedHandoff<T>, RejectedHandoff<T>> {
        match self {
            Self::Accepted(accepted) => Ok(accepted),
            Self::Rejected(rejected) => Err(rejected),
        }
    }
}

/// Backend-side reason a handed-off payload was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HandoffRejectionReason {
    /// The handoff payload did not include token bytes.
    #[error("handoff token is missing")]
    MissingToken,
    /// The handoff payload included token bytes with the wrong length.
    #[error("handoff token length was {actual_len} bytes; expected {expected_len}")]
    InvalidTokenLength {
        /// Presented token byte length.
        actual_len: usize,
        /// Required token byte length.
        expected_len: usize,
    },
    /// The presented token did not match the pending handoff.
    #[error("handoff token mismatch")]
    TokenMismatch,
    /// The pending handoff token exceeded its TTL.
    #[error("handoff token expired")]
    TokenExpired,
    /// The expected handoff token was unknown or already consumed.
    #[error("handoff token is not pending")]
    TokenNotPending,
    /// Unexpected token-store error while accepting a payload.
    #[error("handoff token store error: {error}")]
    TokenStore {
        /// Underlying token-store error.
        error: HandoffTokenError,
    },
}

impl From<HandoffTokenError> for HandoffRejectionReason {
    fn from(value: HandoffTokenError) -> Self {
        match value {
            HandoffTokenError::TokenMismatch => Self::TokenMismatch,
            HandoffTokenError::TokenExpired => Self::TokenExpired,
            HandoffTokenError::TokenNotPending => Self::TokenNotPending,
            error => Self::TokenStore { error },
        }
    }
}

/// Parse raw token bytes into a typed handoff token.
pub fn parse_handoff_token(token: &[u8]) -> Result<HandoffToken, HandoffRejectionReason> {
    if token.is_empty() {
        return Err(HandoffRejectionReason::MissingToken);
    }
    if token.len() != HANDOFF_TOKEN_BYTES {
        return Err(HandoffRejectionReason::InvalidTokenLength {
            actual_len: token.len(),
            expected_len: HANDOFF_TOKEN_BYTES,
        });
    }

    let mut bytes = [0_u8; HANDOFF_TOKEN_BYTES];
    bytes.copy_from_slice(token);
    Ok(HandoffToken::from_bytes(bytes))
}

/// Validate and classify one backend-side handed-off payload.
///
/// On success the pending token is consumed exactly once. Malformed token bytes
/// and mismatches leave the pending token available so the caller can still
/// accept a later correctly-presented payload. Expired tokens are pruned by the
/// token store.
pub fn accept_handed_off<T>(
    pending_tokens: &mut HandoffTokenStore,
    payload: HandedOffPayload<T>,
    now: Instant,
) -> HandoffAcceptance<T> {
    let presented = match parse_handoff_token(payload.presented_token()) {
        Ok(token) => token,
        Err(reason) => return reject(payload, reason),
    };

    match pending_tokens.consume_matching(&payload.expected_token, &presented, now) {
        Ok(()) => HandoffAcceptance::Accepted(AcceptedHandoff {
            token: payload.expected_token,
            connection: payload.connection,
        }),
        Err(error) => reject(payload, error.into()),
    }
}

fn reject<T>(payload: HandedOffPayload<T>, reason: HandoffRejectionReason) -> HandoffAcceptance<T> {
    HandoffAcceptance::Rejected(RejectedHandoff { payload, reason })
}
