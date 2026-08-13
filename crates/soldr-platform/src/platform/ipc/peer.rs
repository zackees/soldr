//! Peer identity and the owner-only Windows pipe the private control
//! endpoint serves.
//!
//! Unix peer identity is resolved during accept (the listener leaf
//! returns it with the connection); this leaf owns the Windows
//! named-pipe server surface: creation with an owner+SYSTEM SDDL,
//! connect, and client-pid observation.

pub use crate::platform_imp::ipc::peer::{
    create_owner_only_windows_pipe, peer_identity_of_pipe_server, pipe_server_connect,
    process_executable, PipeServer,
};
