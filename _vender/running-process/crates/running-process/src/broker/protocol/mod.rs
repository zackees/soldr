//! v1 broker protocol module.
//!
//! Phase 0 of #228 introduced the prost-generated wire types from
//! `proto/broker_v1_*.proto`. Phase 1 (#230) adds the framing
//! read/write helpers used by every connection.
//!
//! All three .proto files share the `running_process.broker.v1`
//! package, so prost-build emits a single Rust module containing
//! every message and enum (Frame, Hello, HelloReply, Refused,
//! Negotiated, AdminRequest, AdminReply, CacheManifest, ServiceDefinition,
//! LifecycleEvent, ...).
//! The prost-generated types are re-exported at the top of this
//! module so existing call sites importing them under
//! `running_process::broker::protocol::*` keep working.

#[allow(missing_docs)]
mod prost_generated {
    include!(concat!(env!("OUT_DIR"), "/running_process.broker.v1.rs"));
}

pub use prost_generated::*;

pub mod frame_ext;
pub mod framing;
pub mod registry;
pub mod validate;

pub use frame_ext::{
    encode_framed, try_decode_framed, DecodedFramed, EndpointNameError, FRAME_HEADER_BYTES,
};
pub use framing::{
    read_frame, read_frame_with_cap, write_frame, FramingError, ENVELOPE_VERSION, MAX_FRAME_BYTES,
    MAX_HELLO_BYTES,
};
pub use registry::{
    ADMIN_PAYLOAD_PROTOCOL, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, CONTROL_PAYLOAD_PROTOCOL,
    FBUILD_PAYLOAD_PROTOCOL, HANDOFF_PAYLOAD_PROTOCOL, PROTOCOL_VERSION, ZCCACHE_PAYLOAD_PROTOCOL,
};
pub use validate::{validate_frame_envelope, FrameValidationError};
