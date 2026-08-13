//! Listener side: control-endpoint claim and accept, plus the
//! owner-only SESSION listener bind.
//!
//! Unix hosts bind a filesystem AF_UNIX socket (0o600, stale-socket
//! reclaim); Windows hosts serve a named-pipe listener pool. The
//! daemon keeps the accept-loop orchestration; these primitives own
//! the OS-specific bind/claim/accept mechanics.

use std::future::Future;
use std::io;
use std::pin::Pin;

use crate::platform::ipc::connect::BoxedAsyncStream;

/// Bind an owner-only local-socket listener at `socket_path` (Unix
/// filesystem path, Windows namespaced pipe) with the same
/// permissions/security the broker's own SESSION bind applies: mode
/// 0o600 on Unix (bind-then-tighten on macOS, where `mode()` is
/// unsupported), an owner+SYSTEM SDDL on Windows.
pub use crate::platform_imp::ipc::listener::bind_owner_only_listener;

/// Claim the private daemon control endpoint at `path`: remove any
/// stale socket, bind, tighten permissions, and capture the socket
/// identity for the retirement fence. On Windows the control endpoint
/// is a named pipe with no filesystem claim; returning
/// `ErrorKind::Unsupported` is correct because the Windows accept loop
/// never calls this.
pub use crate::platform_imp::ipc::listener::claim_control_endpoint_at;

/// File identity of the socket node at `path` (device + inode). The
/// identity is captured right after bind and later used to fence
/// cleanup: a stale daemon unlinks only when the identity still
/// matches, so it can never remove a successor's live socket. Unix
/// hosts only; Windows named pipes have no filesystem identity.
pub use crate::platform_imp::ipc::listener::unix_socket_identity;

/// Remove `path` only when it still carries `expected`'s identity.
/// A `NotFound` is success (nothing to unlink); a mismatched identity
/// leaves the path untouched and reports `Ok(false)`.
pub use crate::platform_imp::ipc::listener::remove_unix_socket_if_matches;

/// Identity of one accepted control connection, observed from the
/// transport (not client-supplied). `is_current_user` gates admission:
/// only the owning user may drive the private control endpoint.
pub struct AcceptedPeer {
    /// The peer process id, when the transport can observe it.
    pub pid: Option<u32>,
    /// The peer executable path, when the transport resolves it (only
    /// the Windows shutdown-request path asks for this).
    pub exe: Option<String>,
    /// Whether the peer belongs to the current user.
    pub is_current_user: bool,
}

/// An accepted control connection: the transport plus its observed
/// identity. The daemon checks the identity, then drives the framed
/// protocol over `stream`.
pub struct AcceptedControlConnection {
    /// The accepted transport stream.
    pub stream: BoxedAsyncStream,
    /// Transport-observed peer identity.
    pub peer: AcceptedPeer,
}

/// File-identity of a bound Unix socket (device + inode), used to fence
/// stale-socket cleanup so a retiring daemon never unlinks a successor's
/// live socket. Windows named pipes have no filesystem identity; the
/// claim that produces one is never reached there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketIdentity {
    /// `st_dev` of the socket node.
    pub device: u64,
    /// `st_ino` of the socket node.
    pub inode: u64,
}

/// A claimed control listener. `accept` yields the next connection with
/// its transport-observed identity; the Windows host never constructs
/// one (its accept loop creates named-pipe instances instead).
pub trait ControlListener: Send + Sync {
    /// Accept the next connection, returning the stream and its
    /// observed identity.
    fn accept(
        &self,
    ) -> Pin<Box<dyn Future<Output = io::Result<AcceptedControlConnection>> + Send + '_>>;
}

/// Boxed [`ControlListener`] as handed to the daemon accept loop.
pub type BoxedControlListener = Box<dyn ControlListener>;
