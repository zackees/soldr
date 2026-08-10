//! Backend integration SDK (#412).
//!
//! The broker v1 post-mortem (issue #412) found that every consumer
//! daemon re-implemented the same five pieces of undifferentiated
//! plumbing: endpoint byte-disambiguation between its legacy wire and
//! running-process frames, `BackendHandle` probe serving, `Frame`
//! construction, request-id correlation, and daemon-identity
//! persistence. This module owns those pieces so a consumer daemon
//! integrates in ~10 lines:
//!
//! - [`BackendEndpointMux`] — sans-io classifier for an accept loop's
//!   read buffer: answers identity probes, hands consumer payload
//!   frames over, and routes legacy-wire bytes back to the consumer's
//!   own decoder. Works under any runtime (sync or async) because it
//!   never performs I/O.
//! - [`FrameClient`] — blocking request/response client with a built-in
//!   request-id counter. Async daemons wrap calls in their runtime's
//!   `spawn_blocking`, exactly like [`BackendHandle::probe_with_service`]
//!   (`running-process`'s `client` feature deliberately has no async
//!   runtime dependency).
//! - [`write_daemon_identity_file`] / [`read_daemon_identity_file`] /
//!   [`remove_daemon_identity_file`] — the JSON identity sidecar that
//!   direct-daemon consumers persist at startup and probe later with
//!   [`BackendHandle::probe_with_service`].
//!
//! Frame construction and buffer codecs live in
//! [`crate::broker::protocol::frame_ext`]; consumer payload-protocol
//! registration lives in [`crate::broker::protocol::registry`] and the
//! [`crate::register_payload_protocol!`] macro. The end-to-end recipe
//! is `docs/INTEGRATE.md`.
//!
//! [`BackendHandle::probe_with_service`]: crate::broker::backend_handle::BackendHandle::probe_with_service

mod frame_client;
#[cfg(feature = "client-async")]
mod frame_client_async;
mod identity_file;
mod mux;

pub use frame_client::{FrameClient, FrameClientError};
#[cfg(feature = "client-async")]
pub use frame_client_async::AsyncFrameClient;
pub use identity_file::{
    read_daemon_identity_file, remove_daemon_identity_file, try_read_daemon_identity_file,
    write_daemon_identity_file,
};
pub use mux::{BackendEndpointMux, LegacyClassification, MuxError, MuxPoll};
