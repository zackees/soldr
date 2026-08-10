#![cfg(all(feature = "client", windows))]

//! Windows end-to-end test for the production handoff orchestration
//! (#354, slice 3).
//!
//! A real child process is spawned and verified as a [`BackendHandle`]
//! (endpoint identity probe, pid liveness, executable identity), then
//! [`execute_verified_windows_handoff`] runs the full sequence for real:
//! `DuplicateHandle` into the child, delivery of the handle value + token
//! over the child-helper protocol (the stand-in for the future wire
//! frame), payload transfer through the duplicated pipe, backend token
//! echo as the acknowledgement, and ACK-registry completion before the
//! deadline.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use interprocess::local_socket::traits::Listener;
use interprocess::local_socket::ListenerOptions;
use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
use running_process::broker::backend_lifecycle::probe::handle_endpoint_probe;
use running_process::broker::protocol::Endpoint;
use running_process::broker::server::handoff::{
    execute_verified_windows_handoff, HandoffAckError, HandoffAckRegistry, HandoffDelivery,
    HandoffDeliveryError, HandoffToken, HandoffTokenStore, PendingHandoffBackend,
    WindowsHandleValue, WindowsHandoffOutcome, HANDOFF_TOKEN_BYTES,
};
use running_process::broker::server::local_socket_name;
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Pipes::CreatePipe;

const CHILD_HELPER_ENV: &str = "RUNNING_PROCESS_ORCHESTRATE_HANDOFF_CHILD";
const CHILD_ENDPOINT_ENV: &str = "RUNNING_PROCESS_ORCHESTRATE_HANDOFF_ENDPOINT";
const CHILD_READY_FILE_ENV: &str = "RUNNING_PROCESS_ORCHESTRATE_HANDOFF_READY_FILE";
const CHILD_HELPER_TEST: &str = "handoff_windows_orchestrate::windows_orchestrate_child_helper";
const CHILD_RESULT_MARKER: &str = "running-process-orchestrate-handoff-child";

/// Delivery channel backed by the cross-process child-helper protocol.
///
/// `deliver` writes the `(handle value, token, payload length)` manifest to
/// the child's stdin and pushes the client payload through the broker-held
/// write end of the pipe. `await_backend_ack` treats the child echoing the
/// exact token back as the backend acknowledgement.
struct ChildHelperDelivery {
    child: ChildProcess,
    write_pipe: Option<std::os::windows::io::OwnedHandle>,
    payload: &'static [u8],
    output: Option<Output>,
}

impl ChildHelperDelivery {
    fn new(
        child: ChildProcess,
        write_pipe: std::os::windows::io::OwnedHandle,
        payload: &'static [u8],
    ) -> Self {
        Self {
            child,
            write_pipe: Some(write_pipe),
            payload,
            output: None,
        }
    }
}

impl HandoffDelivery for ChildHelperDelivery {
    fn deliver(
        &mut self,
        handle: WindowsHandleValue,
        token: &HandoffToken,
    ) -> Result<(), HandoffDeliveryError> {
        use std::os::windows::io::AsRawHandle;

        let manifest = format!(
            "{} {} {}\n",
            handle.get(),
            bytes_to_hex(token.as_bytes()),
            self.payload.len()
        );
        self.child
            .stdin()
            .write_all(manifest.as_bytes())
            .map_err(|err| HandoffDeliveryError::DeliveryFailed {
                detail: format!("manifest write failed: {err}"),
            })?;
        // The child reads stdin to EOF before adopting the handle.
        drop(self.child.take_stdin());

        let write_pipe =
            self.write_pipe
                .take()
                .ok_or_else(|| HandoffDeliveryError::DeliveryFailed {
                    detail: "write pipe already consumed".into(),
                })?;
        let mut written = 0;
        let write_ok = unsafe {
            WriteFile(
                write_pipe.as_raw_handle() as HANDLE,
                self.payload.as_ptr().cast(),
                self.payload.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        drop(write_pipe);
        if write_ok == 0 || written as usize != self.payload.len() {
            return Err(HandoffDeliveryError::DeliveryFailed {
                detail: "payload write through broker pipe failed".into(),
            });
        }
        Ok(())
    }

    fn await_backend_ack(
        &mut self,
        token: &HandoffToken,
        _deadline: Instant,
    ) -> Result<Instant, HandoffDeliveryError> {
        let output = self.child.wait_with_output();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let acknowledged = stdout.contains(&format!(
            "{CHILD_RESULT_MARKER} token={}",
            bytes_to_hex(token.as_bytes())
        ));
        let success = output.status.success();
        self.output = Some(output);
        if !success || !acknowledged {
            return Err(HandoffDeliveryError::AckNotObserved {
                detail: format!("child did not echo the token; stdout:\n{stdout}"),
            });
        }
        Ok(Instant::now())
    }
}

#[test]
fn verified_windows_handoff_orchestration_completes_end_to_end() {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    let payload: &[u8] = b"running-process orchestrated handoff end-to-end";
    let endpoint = child_endpoint();
    let ready_file = child_ready_file();
    let _ = fs::remove_file(&ready_file);

    let mut read_pipe: HANDLE = std::ptr::null_mut();
    let mut write_pipe: HANDLE = std::ptr::null_mut();
    let created = unsafe { CreatePipe(&mut read_pipe, &mut write_pipe, std::ptr::null_mut(), 0) };
    assert_ne!(created, 0, "CreatePipe must create a real pipe pair");
    assert_valid_handle(read_pipe);
    assert_valid_handle(write_pipe);
    let read_pipe = unsafe { OwnedHandle::from_raw_handle(read_pipe.cast()) };
    let write_pipe = unsafe { OwnedHandle::from_raw_handle(write_pipe.cast()) };

    let child = ChildProcess::spawn_with_endpoint(&endpoint.path, &ready_file);
    let child_pid = child.id();
    wait_for_ready_file(&ready_file);

    let daemon = daemon_for_child(child_pid, endpoint.clone());
    let backend =
        BackendHandle::probe_with_service("zccache", "1.11.20", &endpoint, &daemon).unwrap();
    assert_eq!(backend.daemon_process.pid, child_pid);

    // Hello-equivalent orchestration point: issue the one-time token and
    // register the pending ACK against the verified backend pid.
    let issued_at = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let token = tokens.issue(issued_at).unwrap();
    acks.register(
        token,
        PendingHandoffBackend::new("zccache", child_pid),
        issued_at,
    );

    let mut delivery = ChildHelperDelivery::new(child, write_pipe, payload);
    let outcome = execute_verified_windows_handoff(
        &backend,
        WindowsHandleValue::new(read_pipe.as_raw_handle() as usize),
        token,
        &mut tokens,
        &mut acks,
        &mut delivery,
    );
    drop(read_pipe);

    let WindowsHandoffOutcome::Completed(completed) = outcome else {
        panic!("expected completed handoff, got {outcome:?}");
    };
    let _ = fs::remove_file(&ready_file);
    assert_eq!(completed.duplicated.backend_pid, child_pid);
    assert_eq!(completed.duplicated.handoff_token, token);
    assert_eq!(completed.acknowledged.token, token);
    assert_eq!(
        completed.acknowledged.backend,
        PendingHandoffBackend::new("zccache", child_pid)
    );
    assert!(completed.acknowledged.waited < acks.ack_deadline());

    // The child read the payload through the duplicated handle and echoed
    // the exact token alongside it.
    let output = delivery.output.expect("child output must be captured");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = format!(
        "{CHILD_RESULT_MARKER} token={} payload={}",
        bytes_to_hex(token.as_bytes()),
        String::from_utf8_lossy(payload)
    );
    assert!(
        stdout.contains(&expected),
        "child must echo the paired token and transferred payload\nexpected: {expected}\nstdout:\n{stdout}"
    );

    // Exactly-once token consumption: nothing pending, a second ACK and a
    // backend-side replay are both rejected.
    assert_eq!(tokens.pending_len(), 0);
    assert_eq!(acks.pending_len(), 0);
    assert_eq!(
        acks.acknowledge(&mut tokens, &token, Instant::now()),
        Err(HandoffAckError::TokenNotPending)
    );
}

#[test]
#[ignore = "spawned by verified_windows_handoff_orchestration_completes_end_to_end"]
fn windows_orchestrate_child_helper() {
    if std::env::var_os(CHILD_HELPER_ENV).is_none() {
        return;
    }

    if let Some(endpoint_path) = std::env::var_os(CHILD_ENDPOINT_ENV) {
        serve_child_endpoint_probe_once(&endpoint_path.to_string_lossy());
    }

    let mut manifest = String::new();
    std::io::stdin()
        .read_to_string(&mut manifest)
        .expect("child helper must read stdin manifest");
    let manifest = ChildManifest::parse(&manifest);

    let handle = manifest.duplicated_handle as HANDLE;
    assert_valid_handle(handle);
    let token = parse_token_hex(&manifest.token_hex);
    let mut buffer = vec![0_u8; manifest.expected_len];
    let mut total_read = 0;

    while total_read < buffer.len() {
        let mut bytes_read = 0;
        let remaining = &mut buffer[total_read..];
        let read_ok = unsafe {
            ReadFile(
                handle,
                remaining.as_mut_ptr().cast(),
                remaining.len() as u32,
                &mut bytes_read,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(read_ok, 0, "ReadFile must read the duplicated pipe handle");
        assert_ne!(bytes_read, 0, "pipe closed before payload was fully read");
        total_read += bytes_read as usize;
    }

    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(handle);
    }

    let result = format!(
        "{CHILD_RESULT_MARKER} token={} payload={}\n",
        bytes_to_hex(&token),
        String::from_utf8_lossy(&buffer)
    );
    std::io::stdout()
        .write_all(result.as_bytes())
        .expect("child helper must write result");
}

struct ChildProcess {
    child: Option<Child>,
}

impl ChildProcess {
    fn spawn_with_endpoint(endpoint_path: &str, ready_file: &Path) -> Self {
        let child = child_command()
            .env(CHILD_ENDPOINT_ENV, endpoint_path)
            .env(CHILD_READY_FILE_ENV, ready_file)
            .spawn()
            .expect("must spawn orchestrate handoff child helper");
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("child still present").id()
    }

    fn stdin(&mut self) -> &mut std::process::ChildStdin {
        self.child
            .as_mut()
            .expect("child still present")
            .stdin
            .as_mut()
            .expect("child stdin pipe")
    }

    fn take_stdin(&mut self) -> std::process::ChildStdin {
        self.child
            .as_mut()
            .expect("child still present")
            .stdin
            .take()
            .expect("child stdin pipe")
    }

    fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
    }

    fn wait_with_output(&mut self) -> Output {
        self.child
            .take()
            .expect("child still present")
            .wait_with_output()
            .expect("must wait for orchestrate handoff child helper")
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

fn child_command() -> Command {
    let mut command = Command::new(std::env::current_exe().expect("test binary path"));
    command
        .args([
            "--ignored",
            "--exact",
            CHILD_HELPER_TEST,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_HELPER_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn child_endpoint() -> Endpoint {
    Endpoint {
        namespace_id: "verified-child".into(),
        path: format!(
            r"\\.\pipe\rpb-v1-orch-{}-{}",
            std::process::id(),
            unique_suffix()
        ),
    }
}

fn child_ready_file() -> PathBuf {
    std::env::temp_dir().join(format!(
        "running-process-orchestrate-handoff-ready-{}-{}",
        std::process::id(),
        unique_suffix()
    ))
}

fn daemon_for_child(pid: u32, ipc_endpoint: Endpoint) -> DaemonProcess {
    let mut daemon = DaemonProcess::current_process(ipc_endpoint, Some(30)).unwrap();
    daemon.pid = pid;
    daemon
}

fn serve_child_endpoint_probe_once(endpoint_path: &str) {
    let endpoint = Endpoint {
        namespace_id: "verified-child".into(),
        path: endpoint_path.into(),
    };
    let daemon = DaemonProcess::current_process(endpoint.clone(), Some(30)).unwrap();
    let name = local_socket_name(&endpoint.path).unwrap();
    let listener = ListenerOptions::new()
        .name(name)
        .create_sync()
        .expect("child helper must bind endpoint probe socket");
    if let Some(ready_file) = std::env::var_os(CHILD_READY_FILE_ENV) {
        fs::write(PathBuf::from(ready_file), b"ready").expect("child helper must write ready file");
    }
    let mut stream = listener
        .accept()
        .expect("child helper must accept endpoint probe");
    handle_endpoint_probe(&mut stream, &daemon).expect("child helper must answer endpoint probe");
}

fn wait_for_ready_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("child helper did not report endpoint readiness at {path:?}");
}

struct ChildManifest {
    duplicated_handle: usize,
    token_hex: String,
    expected_len: usize,
}

impl ChildManifest {
    fn parse(input: &str) -> Self {
        let mut fields = input.split_whitespace();
        let duplicated_handle = fields
            .next()
            .expect("manifest handle")
            .parse()
            .expect("manifest handle must be usize");
        let token_hex = fields.next().expect("manifest token").to_owned();
        let expected_len = fields
            .next()
            .expect("manifest expected length")
            .parse()
            .expect("manifest expected length must be usize");
        assert!(
            fields.next().is_none(),
            "manifest has unexpected trailing fields"
        );
        Self {
            duplicated_handle,
            token_hex,
            expected_len,
        }
    }
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn parse_token_hex(hex: &str) -> [u8; HANDOFF_TOKEN_BYTES] {
    assert_eq!(hex.len(), HANDOFF_TOKEN_BYTES * 2);
    let mut token = [0_u8; HANDOFF_TOKEN_BYTES];
    for index in 0..HANDOFF_TOKEN_BYTES {
        token[index] = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .expect("token hex must be valid");
    }
    token
}

fn assert_valid_handle(handle: HANDLE) {
    assert!(!handle.is_null());
    assert_ne!(handle, INVALID_HANDLE_VALUE);
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
