//! Backend-facing helpers for v1 broker consumers.
//!
//! This module is intentionally platform-neutral. Windows `DuplicateHandle`
//! and Unix `SCM_RIGHTS` transports will live in platform modules; backend
//! consumers can use this layer to validate and classify handed-off payloads
//! once those transports deliver a candidate connection.

pub mod accept_handed_off;
pub mod wire;

pub use crate::broker::server::{
    HandoffToken, HandoffTokenError, HandoffTokenStore, HandoffTokenStoreConfig,
    DEFAULT_HANDOFF_TOKEN_COLLISION_ATTEMPTS, DEFAULT_HANDOFF_TOKEN_TTL,
    DEFAULT_MAX_PENDING_HANDOFF_TOKENS, HANDOFF_TOKEN_BYTES,
};
pub use accept_handed_off::{
    accept_handed_off, parse_handoff_token, AcceptedHandoff, HandedOffPayload, HandoffAcceptance,
    HandoffRejectionReason, RejectedHandoff,
};
#[allow(deprecated)]
pub use wire::{
    read_handoff_offer, read_handoff_offer_with_deadline, respond_to_handoff_offer,
    serve_handoff_offer, serve_handoff_offer_with_deadline, write_handoff_ack,
    BackendHandoffWireError,
};
