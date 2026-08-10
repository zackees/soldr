#![cfg(all(feature = "client", windows))]

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
    try_duplicate_handle, DuplicateHandleAttempt, HandoffToken, WindowsHandleValue,
    HANDOFF_TOKEN_BYTES,
};
use running_process::broker::server::local_socket_name;
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Pipes::CreatePipe;

const CHILD_HELPER_ENV: &str = "RUNNING_PROCESS_DUP_HANDLE_CHILD";
const CHILD_ENDPOINT_ENV: &str = "RUNNING_PROCESS_DUP_HANDLE_ENDPOINT";
const CHILD_READY_FILE_ENV: &str = "RUNNING_PROCESS_DUP_HANDLE_READY_FILE";
const CHILD_HELPER_TEST: &str =
    "handoff_windows_duplicate_handle::windows_duplicate_handle_child_helper";
const CHILD_RESULT_MARKER: &str = "running-process-duplicate-handle-child";

#[test]
fn windows_duplicate_handle_passes_pipe_to_child_process() {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    let token = HandoffToken::from_bytes([
        0x35, 0x54, 0x11, 0x00, 0x9a, 0xbc, 0xde, 0xf0, 0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc,
        0xfe,
    ]);
    let token_hex = token_to_hex(&token);
    let payload = b"running-process DuplicateHandle cross-process smoke";

    let mut read_pipe: HANDLE = std::ptr::null_mut();
    let mut write_pipe: HANDLE = std::ptr::null_mut();
    let created = unsafe { CreatePipe(&mut read_pipe, &mut write_pipe, std::ptr::null_mut(), 0) };
    assert_ne!(created, 0, "CreatePipe must create a real pipe pair");
    assert_valid_handle(read_pipe);
    assert_valid_handle(write_pipe);

    let read_pipe = unsafe { OwnedHandle::from_raw_handle(read_pipe.cast()) };
    let write_pipe = unsafe { OwnedHandle::from_raw_handle(write_pipe.cast()) };
    let mut child = ChildProcess::spawn();

    let attempt = DuplicateHandleAttempt::new(
        WindowsHandleValue::new(read_pipe.as_raw_handle() as usize),
        child.id(),
        token,
    );
    let duplicated = match try_duplicate_handle(&attempt) {
        Ok(success) => success,
        Err(err) => {
            child.kill();
            panic!("DuplicateHandle into child process failed: {err}");
        }
    };

    assert_eq!(duplicated.backend_pid, child.id());
    assert_eq!(duplicated.handoff_token, token);

    {
        let stdin = child.stdin();
        writeln!(
            stdin,
            "{} {} {}",
            duplicated.duplicated_handle.get(),
            token_hex,
            payload.len()
        )
        .expect("must send duplicated handle manifest to child helper");
    }
    drop(child.take_stdin());
    drop(read_pipe);

    let mut written = 0;
    let write_ok = unsafe {
        WriteFile(
            write_pipe.as_raw_handle() as HANDLE,
            payload.as_ptr().cast(),
            payload.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(write_ok, 0, "WriteFile must write through the pipe");
    assert_eq!(written as usize, payload.len());
    drop(write_pipe);

    let output = child.wait_with_output();
    assert!(
        output.status.success(),
        "child helper failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = format!(
        "{CHILD_RESULT_MARKER} token={token_hex} payload={}",
        String::from_utf8_lossy(payload)
    );
    assert!(
        stdout.contains(&expected),
        "child helper must echo the paired token and transferred payload\nexpected: {expected}\nstdout:\n{stdout}"
    );
}

#[test]
fn backend_handle_windows_duplicate_handle_passes_pipe_to_verified_child_process() {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    let token = HandoffToken::from_bytes([
        0x63, 0x54, 0x11, 0x00, 0x9a, 0xbc, 0xde, 0xf0, 0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc,
        0xfe,
    ]);
    let token_hex = token_to_hex(&token);
    let payload = b"running-process verified BackendHandle DuplicateHandle smoke";
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
    let mut child = ChildProcess::spawn_with_endpoint(&endpoint.path, &ready_file);
    wait_for_ready_file(&ready_file);

    let daemon = daemon_for_child(child.id(), endpoint.clone());
    let backend =
        BackendHandle::probe_with_service("zccache", "1.11.20", &endpoint, &daemon).unwrap();
    let duplicated = match backend.try_duplicate_windows_handoff_handle(
        WindowsHandleValue::new(read_pipe.as_raw_handle() as usize),
        token,
    ) {
        Ok(success) => success,
        Err(err) => {
            child.kill();
            panic!("BackendHandle DuplicateHandle into child process failed: {err}");
        }
    };

    assert_eq!(duplicated.backend_pid, child.id());
    assert_eq!(duplicated.handoff_token, token);

    {
        let stdin = child.stdin();
        writeln!(
            stdin,
            "{} {} {}",
            duplicated.duplicated_handle.get(),
            token_hex,
            payload.len()
        )
        .expect("must send duplicated handle manifest to child helper");
    }
    drop(child.take_stdin());
    drop(read_pipe);

    let mut written = 0;
    let write_ok = unsafe {
        WriteFile(
            write_pipe.as_raw_handle() as HANDLE,
            payload.as_ptr().cast(),
            payload.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(write_ok, 0, "WriteFile must write through the pipe");
    assert_eq!(written as usize, payload.len());
    drop(write_pipe);

    let output = child.wait_with_output();
    let _ = fs::remove_file(&ready_file);
    assert!(
        output.status.success(),
        "child helper failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = format!(
        "{CHILD_RESULT_MARKER} token={token_hex} payload={}",
        String::from_utf8_lossy(payload)
    );
    assert!(
        stdout.contains(&expected),
        "verified child helper must echo the paired token and transferred payload\nexpected: {expected}\nstdout:\n{stdout}"
    );
}

#[test]
#[ignore = "spawned by windows_duplicate_handle_passes_pipe_to_child_process"]
fn windows_duplicate_handle_child_helper() {
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
    fn spawn() -> Self {
        let child = child_command()
            .spawn()
            .expect("must spawn DuplicateHandle child helper");
        Self { child: Some(child) }
    }

    fn spawn_with_endpoint(endpoint_path: &str, ready_file: &Path) -> Self {
        let child = child_command()
            .env(CHILD_ENDPOINT_ENV, endpoint_path)
            .env(CHILD_READY_FILE_ENV, ready_file)
            .spawn()
            .expect("must spawn verified DuplicateHandle child helper");
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
            .expect("must wait for DuplicateHandle child helper")
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
            r"\\.\pipe\rpb-v1-bh-dh-{}-{}",
            std::process::id(),
            unique_suffix()
        ),
    }
}

fn child_ready_file() -> PathBuf {
    std::env::temp_dir().join(format!(
        "running-process-duplicate-handle-ready-{}-{}",
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

impl Drop for ChildProcess {
    fn drop(&mut self) {
        self.kill();
    }
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

fn token_to_hex(token: &HandoffToken) -> String {
    bytes_to_hex(token.as_bytes())
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
