//! Windows listener: the control endpoint is a named-pipe listener
//! pool, not a filesystem claim, so the AF_UNIX claim/accept surface is
//! unsupported; the owner-only SESSION bind uses an SDDL descriptor.

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;

use crate::platform::ipc::listener::{
    AcceptedControlConnection, BoxedControlListener, ControlListener, SocketIdentity,
};

/// Unsupported on Windows: the control endpoint is served by the
/// named-pipe listener pool (see `crate::platform::ipc::peer`), which
/// has no filesystem claim. The daemon reaches this only on the Unix
/// accept path.
pub fn claim_control_endpoint_at(_path: &Path) -> io::Result<(BoxedControlListener, SocketIdentity)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "control-endpoint claim is not supported on Windows",
    ))
}

/// Unsupported on Windows: named pipes have no filesystem identity, so
/// there is nothing to capture. Only the Unix accept path asks for one.
pub fn unix_socket_identity(_path: &Path) -> io::Result<SocketIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unix socket identity is not available on Windows",
    ))
}

/// Unsupported on Windows: named pipes have no filesystem identity to
/// fence cleanup against, and the Unix retirement fence never runs.
pub fn remove_unix_socket_if_matches(_path: &Path, _expected: SocketIdentity) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unix socket cleanup is not supported on Windows",
    ))
}

#[allow(dead_code)] // Unix-path type kept for trait-surface parity; never constructed on Windows.
struct WindowsControlListener;

impl ControlListener for WindowsControlListener {
    fn accept(
        &self,
    ) -> Pin<Box<dyn Future<Output = io::Result<AcceptedControlConnection>> + Send + '_>> {
        Box::pin(async move {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "control-listener accept is not supported on Windows",
            ))
        })
    }
}

/// Bind an owner-only SESSION listener at `socket_path` (a namespaced
/// pipe name on Windows), applying the same owner+SYSTEM SDDL the
/// broker's own SESSION bind uses.
pub fn bind_owner_only_listener(
    socket_path: &str,
) -> io::Result<interprocess::local_socket::tokio::Listener> {
    use interprocess::local_socket::ListenerOptions;
    use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use running_process::broker::server::singleton_bind::wrap_socket_name;

    let name = wrap_socket_name(socket_path).map_err(io::Error::other)?;
    let sddl =
        widestring::U16CString::from_str("D:P(A;;GA;;;OW)(A;;GA;;;SY)").map_err(io::Error::other)?;
    let descriptor = SecurityDescriptor::deserialize(&sddl).map_err(io::Error::other)?;
    ListenerOptions::new()
        .name(name)
        .reclaim_name(false)
        .security_descriptor(descriptor)
        .create_tokio()
}
