//! macOS peer surface: the Windows named-pipe server type does not
//! exist here; every entry is an unsupported stub kept so the facade
//! surface is uniform. Unix peer identity is resolved during accept
//! (see `crate::platform::ipc::listener`).

use std::io;

/// Unsupported on Linux: no named-pipe server type exists. Never
/// constructed at runtime — the Windows accept path is unreachable
/// here — but the daemon's generic accept orchestration compiles
/// against the boxed stream the Windows tree would hand back.
pub type PipeServer = crate::platform::ipc::connect::BoxedAsyncStream;

/// Unsupported on Linux: the private control endpoint is an AF_UNIX
/// listener claimed through [`claim_control_endpoint_at`]
/// (crate::platform::ipc::listener).
pub fn create_owner_only_windows_pipe(_endpoint: &str, _first: bool) -> io::Result<PipeServer> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "named-pipe servers do not exist on macOS",
    ))
}

/// Unsupported on macOS; unreachable at runtime.
pub async fn pipe_server_connect(_server: &mut PipeServer) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "named-pipe connect does not exist on macOS",
    ))
}

/// Unsupported on macOS; unreachable at runtime. Unix peer identity is
/// resolved during accept.
pub fn peer_identity_of_pipe_server(_server: &mut PipeServer) -> Option<u32> {
    None
}

/// Unsupported on macOS; unreachable at runtime. The Unix shutdown-request
/// path does not resolve the requester executable.
pub fn process_executable(_pid: u32) -> Option<String> {
    None
}
