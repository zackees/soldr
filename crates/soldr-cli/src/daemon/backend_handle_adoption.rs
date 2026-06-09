//! soldr-daemon integration with `running-process` `BackendHandle`.
//!
//! `running-process` commit 04f6387 added the active endpoint-response probe
//! required by zackees/running-process#232. soldr now uses that probe at the
//! old `daemon::lifecycle::is_live` boundary: the PID file is only trusted
//! after `BackendHandle::probe_with_service` verifies process identity and the
//! live daemon answers the broker-v1 BackendHandle nonce challenge on its IPC
//! endpoint.

use crate::cache_lib::daemon_pid_path;
use crate::core::SoldrPaths;
#[cfg(unix)]
use crate::daemon::client;
use crate::daemon::lifecycle::{pid_exe_stem_matches, pid_is_alive, read_pid_file};
use crate::daemon::protocol::PROTOCOL_VERSION;
use prost14::Message as _;
use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
use running_process::broker::backend_lifecycle::identity::IdentityError;
use running_process::broker::backend_lifecycle::probe::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL;
use running_process::broker::host_identity;
use running_process::broker::protocol::{
    Endpoint, Frame, FrameKind, PayloadEncoding, ENVELOPE_VERSION, MAX_FRAME_BYTES,
};
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const BACKEND_HANDLE_PROBE_PREFIX_BYTES: usize = 5;
const BACKEND_HANDLE_PROBE_NONCE_BYTES: usize = 32;

pub(crate) const SOLDR_DAEMON_SERVICE_NAME: &str = "soldr-daemon";
pub(crate) const SOLDR_DAEMON_SERVICE_VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const RUNNING_PROCESS_DISABLE_ENV: &str = "RUNNING_PROCESS_DISABLE";

pub(crate) const RUNNING_PROCESS_BACKEND_HANDLE_STATUS: RunningProcessBackendHandleStatus =
    RunningProcessBackendHandleStatus {
        crate_name: "running-process",
        dependency_source:
            "git:https://github.com/zackees/running-process#04f6387c3cf5b2a984cd4bbba8a3e6f177d43a89",
        required_symbol: "running_process::broker::backend_handle::BackendHandle",
        running_process_issue: "zackees/running-process#232",
        adoption_tracker_issue: "zackees/running-process#242",
        soldr_issue: "zackees/soldr#718",
        active_endpoint_probe: true,
        remaining_gate:
            "publish running-process with BackendHandle and collect three-OS downstream acceptance",
    };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunningProcessBackendHandleStatus {
    pub(crate) crate_name: &'static str,
    pub(crate) dependency_source: &'static str,
    pub(crate) required_symbol: &'static str,
    pub(crate) running_process_issue: &'static str,
    pub(crate) adoption_tracker_issue: &'static str,
    pub(crate) soldr_issue: &'static str,
    pub(crate) active_endpoint_probe: bool,
    pub(crate) remaining_gate: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SoldrDaemonBackendHandle {
    pub(crate) service_name: &'static str,
    pub(crate) service_version: &'static str,
    pub(crate) protocol_version: u32,
    pub(crate) pid: u32,
    pub(crate) exe_path: PathBuf,
    pub(crate) endpoint: PathBuf,
    pub(crate) pid_file: PathBuf,
    pub(crate) adoption_status: RunningProcessBackendHandleStatus,
}

impl SoldrDaemonBackendHandle {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn is_alive(&self) -> bool {
        pid_is_alive(self.pid) && pid_exe_stem_matches(self.pid, SOLDR_DAEMON_SERVICE_NAME)
    }
}

pub(crate) fn running_process_disabled() -> bool {
    std::env::var(RUNNING_PROCESS_DISABLE_ENV).is_ok_and(|value| value == "1")
}

pub(crate) fn probe_soldr_daemon(paths: &SoldrPaths) -> Option<SoldrDaemonBackendHandle> {
    let (pid, exe_path) = read_pid_file(paths)?;
    let expected = daemon_process_from_pid_file(paths, pid, exe_path)?;
    let handle = BackendHandle::probe_with_service(
        SOLDR_DAEMON_SERVICE_NAME,
        SOLDR_DAEMON_SERVICE_VERSION,
        &expected.ipc_endpoint,
        &expected,
    )
    .ok()?;

    Some(SoldrDaemonBackendHandle {
        service_name: SOLDR_DAEMON_SERVICE_NAME,
        service_version: SOLDR_DAEMON_SERVICE_VERSION,
        protocol_version: PROTOCOL_VERSION,
        pid: handle.daemon_process.pid,
        exe_path: handle.daemon_process.exe_path,
        endpoint: PathBuf::from(handle.daemon_process.ipc_endpoint.path),
        pid_file: daemon_pid_path(paths),
        adoption_status: RUNNING_PROCESS_BACKEND_HANDLE_STATUS,
    })
}

pub(crate) fn current_daemon_process(
    paths: &SoldrPaths,
    idle_timeout_secs: Option<u32>,
) -> Result<DaemonProcess, IdentityError> {
    DaemonProcess::current_process(soldr_daemon_endpoint(paths), idle_timeout_secs)
}

pub(crate) fn is_backend_handle_probe_prefix(prefix: &[u8]) -> bool {
    if prefix.len() != BACKEND_HANDLE_PROBE_PREFIX_BYTES || prefix[0] != ENVELOPE_VERSION {
        return false;
    }
    let body_len = u32::from_le_bytes([prefix[1], prefix[2], prefix[3], prefix[4]]) as usize;
    body_len <= MAX_FRAME_BYTES
}

pub(crate) async fn handle_backend_handle_probe_async<S>(
    stream: &mut S,
    prefix: &[u8],
    daemon: &DaemonProcess,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request_frame = read_backend_handle_probe_request(stream, prefix).await?;
    let nonce = validate_backend_handle_probe_request(&request_frame)?;
    write_backend_handle_probe_response(stream, &request_frame, &nonce, daemon).await
}

fn daemon_process_from_pid_file(
    paths: &SoldrPaths,
    pid: u32,
    exe_path: PathBuf,
) -> Option<DaemonProcess> {
    Some(DaemonProcess {
        pid,
        exe_sha256: sha256_file(&exe_path).ok()?,
        exe_path,
        boot_id: host_identity::current().boot_id,
        ipc_endpoint: soldr_daemon_endpoint(paths),
        started_at_unix_ms: 0,
        idle_timeout_secs: None,
    })
}

fn soldr_daemon_endpoint(paths: &SoldrPaths) -> Endpoint {
    Endpoint {
        namespace_id: host_identity::current().namespace_id,
        path: soldr_daemon_endpoint_path(paths),
    }
}

#[cfg(unix)]
fn soldr_daemon_endpoint_path(paths: &SoldrPaths) -> String {
    client::default_sock_path(paths)
        .to_string_lossy()
        .into_owned()
}

#[cfg(windows)]
fn soldr_daemon_endpoint_path(paths: &SoldrPaths) -> String {
    crate::cache_lib::daemon_pipe_name(paths)
}

async fn read_backend_handle_probe_request<S>(stream: &mut S, prefix: &[u8]) -> io::Result<Frame>
where
    S: AsyncRead + Unpin,
{
    if !is_backend_handle_probe_prefix(prefix) {
        return Err(invalid_data(
            "not a running-process BackendHandle probe frame",
        ));
    }
    let body_len = u32::from_le_bytes([prefix[1], prefix[2], prefix[3], prefix[4]]) as usize;
    let mut body = vec![0_u8; body_len];
    if body_len > 0 {
        stream.read_exact(&mut body).await?;
    }
    Frame::decode(body.as_slice()).map_err(|err| invalid_data(err.to_string()))
}

fn validate_backend_handle_probe_request(frame: &Frame) -> io::Result<[u8; 32]> {
    if frame.envelope_version != ENVELOPE_VERSION as u32 {
        return Err(invalid_data(
            "BackendHandle probe envelope_version is not v1",
        ));
    }
    if FrameKind::try_from(frame.kind) != Ok(FrameKind::Request) {
        return Err(invalid_data("BackendHandle probe kind is not REQUEST"));
    }
    if frame.payload_protocol != BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL {
        return Err(invalid_data(
            "BackendHandle probe payload_protocol does not match",
        ));
    }
    if PayloadEncoding::try_from(frame.payload_encoding) != Ok(PayloadEncoding::None) {
        return Err(invalid_data("BackendHandle probe payload is compressed"));
    }
    frame
        .payload
        .as_slice()
        .try_into()
        .map_err(|_| invalid_data("BackendHandle probe nonce must be 32 bytes"))
}

async fn write_backend_handle_probe_response<S>(
    stream: &mut S,
    request_frame: &Frame,
    nonce: &[u8; 32],
    daemon: &DaemonProcess,
) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut payload = Vec::with_capacity(BACKEND_HANDLE_PROBE_NONCE_BYTES + 256);
    payload.extend_from_slice(nonce);
    daemon
        .to_proto()
        .encode(&mut payload)
        .map_err(|err| invalid_data(err.to_string()))?;

    let response = Frame {
        envelope_version: ENVELOPE_VERSION as u32,
        kind: FrameKind::Response as i32,
        payload_protocol: BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
        payload,
        request_id: request_frame.request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: request_frame.traceparent.clone(),
        tracestate: request_frame.tracestate.clone(),
    };

    let mut body = Vec::with_capacity(response.encoded_len());
    response
        .encode(&mut body)
        .map_err(|err| invalid_data(err.to_string()))?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(invalid_data(
            "BackendHandle probe response exceeds frame cap",
        ));
    }

    let mut wire = Vec::with_capacity(BACKEND_HANDLE_PROBE_PREFIX_BYTES + body.len());
    wire.push(ENVELOPE_VERSION);
    wire.extend_from_slice(&(body.len() as u32).to_le_bytes());
    wire.extend_from_slice(&body);
    stream.write_all(&wire).await?;
    stream.flush().await
}

fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    let bytes = std::fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_lib::soldr_daemon_dir;
    use tempfile::TempDir;

    fn write_pid_file(paths: &SoldrPaths, pid: u32, exe_path: &Path) {
        std::fs::create_dir_all(soldr_daemon_dir(paths)).expect("daemon dir");
        std::fs::write(
            daemon_pid_path(paths),
            format!("{pid}\n{}\n", exe_path.display()),
        )
        .expect("write pid file");
    }

    #[test]
    fn dependency_status_documents_active_backend_handle_usage() {
        let status = RUNNING_PROCESS_BACKEND_HANDLE_STATUS;
        assert_eq!(status.crate_name, "running-process");
        assert!(status.dependency_source.contains("04f6387c3cf5b2a984"));
        assert_eq!(
            status.required_symbol,
            "running_process::broker::backend_handle::BackendHandle"
        );
        assert_eq!(status.running_process_issue, "zackees/running-process#232");
        assert_eq!(status.adoption_tracker_issue, "zackees/running-process#242");
        assert_eq!(status.soldr_issue, "zackees/soldr#718");
        assert!(status.active_endpoint_probe);
        assert!(status.remaining_gate.contains("three-OS"));
    }

    #[test]
    fn running_process_disable_requires_exact_one() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let prior = std::env::var_os(RUNNING_PROCESS_DISABLE_ENV);

        std::env::remove_var(RUNNING_PROCESS_DISABLE_ENV);
        assert!(!running_process_disabled());

        std::env::set_var(RUNNING_PROCESS_DISABLE_ENV, "true");
        assert!(!running_process_disabled());

        std::env::set_var(RUNNING_PROCESS_DISABLE_ENV, "1");
        assert!(running_process_disabled());

        match prior {
            Some(value) => std::env::set_var(RUNNING_PROCESS_DISABLE_ENV, value),
            None => std::env::remove_var(RUNNING_PROCESS_DISABLE_ENV),
        }
    }

    #[test]
    fn probe_missing_pid_file_reports_no_handle() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        assert!(probe_soldr_daemon(&paths).is_none());
    }

    #[test]
    fn probe_stale_pid_file_reports_no_handle() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        write_pid_file(&paths, u32::MAX, Path::new("soldr-daemon"));

        assert!(probe_soldr_daemon(&paths).is_none());
    }

    #[test]
    fn pid_file_identity_records_running_process_backend_handle_shape() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let current_exe = std::env::current_exe().expect("current exe");

        let identity =
            daemon_process_from_pid_file(&paths, std::process::id(), current_exe.clone())
                .expect("identity");

        assert_eq!(identity.pid, std::process::id());
        assert_eq!(identity.exe_path, current_exe);
        assert_eq!(identity.ipc_endpoint, soldr_daemon_endpoint(&paths));
        assert_eq!(identity.exe_sha256, sha256_file(&current_exe).unwrap());
        assert!(!identity.boot_id.is_empty());
    }

    #[test]
    fn backend_handle_probe_prefix_classifies_broker_v1_only() {
        let mut running_process_prefix = [0_u8; BACKEND_HANDLE_PROBE_PREFIX_BYTES];
        running_process_prefix[0] = ENVELOPE_VERSION;
        running_process_prefix[1..].copy_from_slice(&16_u32.to_le_bytes());
        assert!(is_backend_handle_probe_prefix(&running_process_prefix));

        let mut soldr_prefix = [0_u8; BACKEND_HANDLE_PROBE_PREFIX_BYTES];
        soldr_prefix[..4].copy_from_slice(&1_u32.to_le_bytes());
        soldr_prefix[4] = PROTOCOL_VERSION as u8;
        assert!(!is_backend_handle_probe_prefix(&soldr_prefix));
    }
}
