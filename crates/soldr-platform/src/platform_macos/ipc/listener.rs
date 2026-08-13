//! macOS listener: AF_UNIX control-endpoint claim and accept with
//! peer-credential admission, plus the owner-only SESSION listener
//! bind (bind first, then tighten to 0o600 — interprocess `mode()`
//! returns Unsupported on macOS).

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;

use crate::platform::ipc::listener::{
    AcceptedControlConnection, AcceptedPeer, BoxedControlListener, ControlListener, SocketIdentity,
};

/// Claim the private control endpoint at `sock`: private parent
/// directory, stale-socket unlink, bind, 0o600, then capture the socket
/// identity used to fence retiring-daemon cleanup. On identity-capture
/// failure the freshly bound socket is removed so a successor can claim
/// the endpoint cleanly.
pub fn claim_control_endpoint_at(sock: &Path) -> io::Result<(BoxedControlListener, SocketIdentity)> {
    if let Some(parent) = sock.parent() {
        running_process::broker::secure_dir::ensure_private_dir(parent)?;
    }
    match std::fs::remove_file(sock) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = tokio::net::UnixListener::bind(sock)?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600))?;
    }
    let identity = match unix_socket_identity(sock) {
        Ok(identity) => identity,
        Err(error) => {
            drop(listener);
            let _ = std::fs::remove_file(sock);
            return Err(error);
        }
    };
    Ok((Box::new(UnixControlListener { listener }), identity))
}

/// File identity of the socket node at `path` (device + inode), used
/// to fence cleanup so a stale daemon never removes a successor's live
/// socket.
pub fn unix_socket_identity(path: &Path) -> io::Result<SocketIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Remove `path` only when it still carries `expected`'s identity.
/// A `NotFound` is success (nothing to unlink); a mismatched identity
/// leaves the path untouched and reports `Ok(false)`.
pub fn remove_unix_socket_if_matches(path: &Path, expected: SocketIdentity) -> io::Result<bool> {
    let actual = match unix_socket_identity(path) {
        Ok(actual) => actual,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if actual != expected {
        return Ok(false);
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

struct UnixControlListener {
    listener: tokio::net::UnixListener,
}

impl ControlListener for UnixControlListener {
    fn accept(
        &self,
    ) -> Pin<Box<dyn Future<Output = io::Result<AcceptedControlConnection>> + Send + '_>> {
        Box::pin(async move {
            let (stream, _addr) = self.listener.accept().await?;
            // Transport-observed identity: pid from peer credentials,
            // and the same credentials gate current-user admission.
            // Computed before boxing because only the concrete type
            // exposes them.
            let credentials = stream.peer_cred().ok();
            let pid = credentials
                .as_ref()
                .and_then(|credentials| credentials.pid())
                .and_then(|pid| u32::try_from(pid).ok());
            let is_current_user = credentials
                .as_ref()
                .is_some_and(|credentials| credentials.uid() == unsafe { libc::geteuid() });
            Ok(AcceptedControlConnection {
                stream: Box::new(stream),
                peer: AcceptedPeer {
                    pid,
                    exe: None,
                    is_current_user,
                },
            })
        })
    }
}

/// Bind an owner-only SESSION listener at `socket_path` (a filesystem
/// path on macOS). interprocess implements `mode()` with fchmod before
/// bind, which returns Unsupported on macOS — so bind first, then
/// tighten the filesystem entry to 0o600, and reclaim a stale socket
/// exactly like the broker's own SESSION bind.
pub fn bind_owner_only_listener(
    socket_path: &str,
) -> io::Result<interprocess::local_socket::tokio::Listener> {
    use interprocess::local_socket::ListenerOptions;
    use running_process::broker::server::singleton_bind::{
        is_already_bound_error, unix_socket_path_is_stale, wrap_socket_name,
    };
    use std::os::unix::fs::PermissionsExt as _;

    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        running_process::broker::secure_dir::ensure_private_dir(parent)?;
    }

    let bind = |path: &str| {
        ListenerOptions::new()
            .name(wrap_socket_name(path).map_err(io::Error::other)?)
            .reclaim_name(false)
            .create_tokio()
    };
    let listener = match bind(socket_path) {
        Ok(listener) => listener,
        Err(err) if is_already_bound_error(&err) && unix_socket_path_is_stale(socket_path) => {
            let _ = std::fs::remove_file(socket_path);
            bind(socket_path)?
        }
        Err(err) => return Err(err),
    };
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] // allow-bare-test: soldr-platform is a dependency leaf (#2493)
    fn session_listener_creates_missing_parent_and_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _context = runtime.enter();
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path().join("missing").join("runtime");
        let socket = parent.join("daemon.session");
        let listener = bind_owner_only_listener(&socket.display().to_string())
            .expect("bind owner-only listener");
        assert!(parent.is_dir());
        assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
        drop(listener);
    }

    #[test] // allow-bare-test: soldr-platform is a dependency leaf (#2493)
    fn endpoint_retirement_is_identity_fenced() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _context = runtime.enter();
        let temp = tempfile::tempdir().expect("tempdir");
        let socket = temp.path().join("control.sock");
        let (old_listener, old_identity) = claim_control_endpoint_at(&socket).expect("old endpoint");
        std::fs::remove_file(&socket).expect("unlink old endpoint name");
        let (replacement_listener, replacement_identity) =
            claim_control_endpoint_at(&socket).expect("replacement endpoint");
        assert_ne!(old_identity, replacement_identity);
        assert!(!remove_unix_socket_if_matches(&socket, old_identity).unwrap());
        assert!(socket.exists());
        assert!(remove_unix_socket_if_matches(&socket, replacement_identity).unwrap());
        drop(replacement_listener);
        drop(old_listener);
    }
}
