//! Normalized daemon identity carried by `BackendHandle`.
//!
//! `DaemonProcess` is the typed form of `CacheManifest.current_daemon`. It is
//! deliberately more specific than the generated protobuf message: paths are
//! `PathBuf`s, executable hashes are fixed 32-byte arrays, and the IPC endpoint
//! is required. That keeps malformed manifests out of the `BackendHandle` probe
//! path.

use std::convert::TryFrom;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::broker::host_identity;
use crate::broker::protocol::{self, CacheManifest, Endpoint};

/// A backend daemon identity with fixed-width fields suitable for verification.
///
/// This mirrors `CacheManifest.current_daemon`, but normalizes protobuf strings
/// and byte vectors into path and digest types that are harder to misuse.
///
/// Persist this value only after the daemon has selected its final IPC endpoint
/// and executable. Later consumers can pass the same identity to
/// [`crate::broker::backend_handle::BackendHandle::probe`] or store it as
/// `CacheManifest.current_daemon`.
///
/// ```no_run
/// use running_process::broker::backend_handle::DaemonProcess;
/// use running_process::broker::protocol::{CacheManifest, Endpoint};
///
/// # fn example(mut manifest: CacheManifest)
/// #     -> Result<CacheManifest, running_process::broker::backend_lifecycle::identity::IdentityError>
/// # {
/// let endpoint = Endpoint {
///     namespace_id: "host-namespace".to_owned(),
///     path: "running-process-backend.sock".to_owned(),
/// };
/// let daemon = DaemonProcess::current_process(endpoint, Some(600))?;
///
/// manifest.current_daemon = Some(daemon.to_proto());
/// # Ok(manifest)
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonProcess {
    /// Operating-system process ID.
    pub pid: u32,
    /// Executable path recorded when the daemon identity was written.
    pub exe_path: PathBuf,
    /// SHA-256 of the daemon executable.
    pub exe_sha256: [u8; 32],
    /// Host boot ID observed when the daemon started.
    pub boot_id: String,
    /// IPC endpoint used to connect to the daemon.
    pub ipc_endpoint: Endpoint,
    /// Daemon start timestamp in Unix milliseconds.
    pub started_at_unix_ms: u64,
    /// Optional idle timeout advertised by the daemon.
    pub idle_timeout_secs: Option<u32>,
}

impl DaemonProcess {
    /// Build a daemon identity for the current process.
    ///
    /// This is primarily useful for tests and direct-daemon consumers that have
    /// just spawned a backend and need to persist a manifest entry.
    ///
    /// The executable digest is taken from `std::env::current_exe()` at the time
    /// this method runs. If a daemon relocates or replaces its executable after
    /// startup, record the final identity after relocation instead.
    pub fn current_process(
        ipc_endpoint: Endpoint,
        idle_timeout_secs: Option<u32>,
    ) -> Result<Self, IdentityError> {
        let exe_path = std::env::current_exe().map_err(IdentityError::CurrentExe)?;
        let exe_sha256 = sha256_file(&exe_path)?;
        Ok(Self {
            pid: std::process::id(),
            exe_path,
            exe_sha256,
            boot_id: host_identity::current().boot_id,
            ipc_endpoint,
            started_at_unix_ms: unix_now_ms(),
            idle_timeout_secs,
        })
    }

    /// Convert this identity into the protobuf form stored in `CacheManifest`.
    ///
    /// The conversion preserves the fixed-width SHA-256 value as bytes for the
    /// wire schema.
    pub fn to_proto(&self) -> protocol::DaemonProcess {
        protocol::DaemonProcess {
            pid: self.pid,
            exe_path: self.exe_path.to_string_lossy().into_owned(),
            exe_sha256: self.exe_sha256.to_vec(),
            ipc_endpoint: Some(self.ipc_endpoint.clone()),
            started_at_unix_ms: self.started_at_unix_ms,
            boot_id: self.boot_id.clone(),
            idle_timeout_secs: self.idle_timeout_secs,
        }
    }

    /// Read and normalize `CacheManifest.current_daemon`.
    ///
    /// Returns `Ok(None)` when the manifest has no daemon entry. Malformed
    /// entries, such as a missing endpoint or non-32-byte executable digest,
    /// return an [`IdentityError`].
    pub fn from_manifest_current_daemon(
        manifest: &CacheManifest,
    ) -> Result<Option<Self>, IdentityError> {
        manifest
            .current_daemon
            .clone()
            .map(Self::try_from)
            .transpose()
    }
}

impl TryFrom<protocol::DaemonProcess> for DaemonProcess {
    type Error = IdentityError;

    fn try_from(value: protocol::DaemonProcess) -> Result<Self, Self::Error> {
        let ipc_endpoint = value.ipc_endpoint.ok_or(IdentityError::MissingEndpoint)?;
        let exe_sha256 =
            vec_to_sha256(value.exe_sha256).map_err(IdentityError::InvalidSha256Length)?;
        Ok(Self {
            pid: value.pid,
            exe_path: PathBuf::from(value.exe_path),
            exe_sha256,
            boot_id: value.boot_id,
            ipc_endpoint,
            started_at_unix_ms: value.started_at_unix_ms,
            idle_timeout_secs: value.idle_timeout_secs,
        })
    }
}

impl From<&DaemonProcess> for protocol::DaemonProcess {
    fn from(value: &DaemonProcess) -> Self {
        value.to_proto()
    }
}

impl Serialize for DaemonProcess {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        DaemonProcessSerde::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DaemonProcess {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = DaemonProcessSerde::deserialize(deserializer)?;
        Ok(Self {
            pid: value.pid,
            exe_path: value.exe_path,
            exe_sha256: value.exe_sha256,
            boot_id: value.boot_id,
            ipc_endpoint: value.ipc_endpoint.into(),
            started_at_unix_ms: value.started_at_unix_ms,
            idle_timeout_secs: value.idle_timeout_secs,
        })
    }
}

/// Errors returned while normalizing daemon identity.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The protobuf daemon identity did not include an IPC endpoint.
    #[error("daemon process is missing ipc_endpoint")]
    MissingEndpoint,
    /// The protobuf daemon identity had an executable digest with the wrong size.
    #[error("daemon process exe_sha256 must be 32 bytes, got {0}")]
    InvalidSha256Length(usize),
    /// The current executable path could not be read.
    #[error("failed to resolve current executable: {0}")]
    CurrentExe(io::Error),
    /// A filesystem operation failed while hashing the executable.
    #[error("failed to hash executable: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonProcessSerde {
    pid: u32,
    exe_path: PathBuf,
    exe_sha256: [u8; 32],
    boot_id: String,
    ipc_endpoint: EndpointSerde,
    started_at_unix_ms: u64,
    idle_timeout_secs: Option<u32>,
}

impl From<&DaemonProcess> for DaemonProcessSerde {
    fn from(value: &DaemonProcess) -> Self {
        Self {
            pid: value.pid,
            exe_path: value.exe_path.clone(),
            exe_sha256: value.exe_sha256,
            boot_id: value.boot_id.clone(),
            ipc_endpoint: EndpointSerde::from(&value.ipc_endpoint),
            started_at_unix_ms: value.started_at_unix_ms,
            idle_timeout_secs: value.idle_timeout_secs,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EndpointSerde {
    namespace_id: String,
    path: String,
}

impl From<&Endpoint> for EndpointSerde {
    fn from(value: &Endpoint) -> Self {
        Self {
            namespace_id: value.namespace_id.clone(),
            path: value.path.clone(),
        }
    }
}

impl From<EndpointSerde> for Endpoint {
    fn from(value: EndpointSerde) -> Self {
        Endpoint {
            namespace_id: value.namespace_id,
            path: value.path,
        }
    }
}

pub fn sha256_file(path: &Path) -> Result<[u8; 32], io::Error> {
    let bytes = fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

fn vec_to_sha256(bytes: Vec<u8>) -> Result<[u8; 32], usize> {
    let len = bytes.len();
    let Ok(out) = bytes.try_into() else {
        return Err(len);
    };
    Ok(out)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
