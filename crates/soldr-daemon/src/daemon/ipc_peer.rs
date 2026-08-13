//! OS-observed metadata for an accepted daemon-control connection.
//!
//! The transport-observed identity comes from the platform ipc facade:
//! Unix resolves pid + current-user admission during accept (the
//! listener leaf), Windows observes the client pid on the named-pipe
//! server. This module owns only the neutral `PeerIdentity` shape and
//! the shutdown-request attribution.

use crate::daemon::lifecycle::LifecycleSource;

/// Transport-observed identity of one accepted IPC peer.
///
/// This is deliberately not carried in the wire request: Windows can identify
/// the process that owns the other end of the pipe, so trusting client-supplied
/// fields would weaken the attribution. Unix stays explicitly unknown until
/// the transport exposes credentials we can obtain without inventing them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    pub pid: Option<u32>,
    pub exe: Option<String>,
    pub source: LifecycleSource,
}

impl PeerIdentity {
    pub fn unknown() -> Self {
        Self {
            pid: None,
            exe: None,
            source: LifecycleSource::Unknown,
        }
    }

    /// Identity observed by the transport during accept (the platform
    /// listener leaf resolves pid and current-user admission before the
    /// connection is handed to the daemon).
    pub fn from_accepted_peer(peer: &crate::platform::ipc::listener::AcceptedPeer) -> Self {
        Self {
            pid: peer.pid,
            exe: peer.exe.clone(),
            source: peer
                .pid
                .map(|_| LifecycleSource::IpcPeer)
                .unwrap_or(LifecycleSource::Unknown),
        }
    }

    /// Persist requester attribution before the daemon writes its shutdown ACK.
    pub fn record_shutdown_requested(self, paths: &crate::core::SoldrPaths, generation: u64) {
        use crate::daemon::lifecycle::{
            append_lifecycle_event_with, LifecycleDetails, LifecycleReason,
        };
        // Only the Windows transport resolves the requesting executable;
        // the exe is resolved lazily here so the hot accept path never
        // pays for it.
        let mut peer = self;
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
            && peer.exe.is_none()
        {
            peer.exe = peer
                .pid
                .and_then(crate::platform::ipc::peer::process_executable);
        }
        append_lifecycle_event_with(
            paths,
            "shutdown-requested",
            LifecycleDetails::requested(LifecycleReason::ExplicitStop)
                .for_target_generation(std::process::id(), generation)
                .with_peer(peer),
        );
    }

    /// Identity of a connected Windows named-pipe server instance
    /// (`GetNamedPipeClientProcessId`, best-effort telemetry). Never
    /// reached on Unix hosts, whose accept loop builds the identity via
    /// [`Self::from_accepted_peer`].
    pub fn from_windows_pipe_server(server: &mut crate::platform::ipc::peer::PipeServer) -> Self {
        match crate::platform::ipc::peer::peer_identity_of_pipe_server(server) {
            Some(pid) => Self {
                pid: Some(pid),
                exe: None,
                source: LifecycleSource::IpcPeer,
            },
            None => Self::unknown(),
        }
    }
}
