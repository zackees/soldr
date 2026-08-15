//! Daemon wire schema — the pure-data half (issue #1490 Phase 0, edge E2).
//!
//! Owns the hand-written prost message types ([`proto`], schema
//! documented next door in [`wire.proto`](./wire.proto)), the redb
//! row-tag helpers, and [`WireDecodeError`]. Moved here from
//! `daemon::wire` so `cache_lib` (which persists prost-tagged redb
//! rows via `cook_index`) does not need an upward edge into `daemon`.
//!
//! The Rust ↔ wire conversions between these messages and the daemon's
//! public `Request` / `Response` types stay in `crate::daemon::wire`,
//! which re-exports everything here at its old paths so daemon-side
//! callers compile unchanged. Prost message types are pure data —
//! consistent with core's "no I/O beyond config files" rule.

use thiserror::Error;

#[path = "wire_proto.rs"]
pub mod proto;

/// Errors surfaced when decoding wire bytes into the public Rust
/// types. Wraps prost decode failures plus the shape-validation cases
/// proto3 cannot express (fixed-length byte arrays, non-empty oneofs).
#[derive(Debug, Error)]
pub enum WireDecodeError {
    #[error("prost decode error: {0}")]
    Prost(#[from] prost::DecodeError),
    #[error("invalid SHA-256 length on the wire: expected 32 bytes, got {0}")]
    InvalidShaLength(usize),
    #[error("empty {0} oneof — payload missing a discriminant")]
    EmptyOneof(&'static str),
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("unknown event kind discriminant: {0}")]
    UnknownEventKind(u32),
}

// =========================================================================
// Wire-tagged byte for persistent state-store rows
// =========================================================================

/// The 0x01 byte that prefixes every prost-encoded state-store row
/// written by this codebase (named for the redb era; the encoding
/// survived the SQLite migration verbatim). Reads look for it; absence
/// (or any other byte) is rejected as invalid data.
pub const REDB_TAG_PROST: u8 = 0x01;

/// Prepend [`REDB_TAG_PROST`] to a prost-encoded body. Used by every
/// writer that lands a row into a redb table participating in the
/// #580 migration.
pub fn prost_tagged_bytes<M: prost::Message>(message: &M) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + message.encoded_len());
    out.push(REDB_TAG_PROST);
    message.encode(&mut out).expect("Vec write is infallible");
    out
}
