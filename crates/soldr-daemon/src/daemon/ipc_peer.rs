//! OS-observed metadata for an accepted Windows named-pipe connection.

use crate::daemon::lifecycle::LifecycleSource;

/// Transport-observed identity of one accepted IPC peer.
///
/// This is deliberately not carried in the wire request: Windows can identify
/// the process that owns the other end of the pipe, so trusting client-supplied
/// fields would weaken the attribution. Unix stays explicitly unknown until
/// the transport exposes credentials we can obtain without inventing them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PeerIdentity {
    pub(crate) pid: Option<u32>,
    pub(crate) exe: Option<String>,
    pub(crate) source: LifecycleSource,
}

impl PeerIdentity {
    pub(crate) fn unknown() -> Self {
        Self {
            pid: None,
            exe: None,
            source: LifecycleSource::Unknown,
        }
    }

    #[cfg(unix)]
    pub(crate) fn from_unix_stream(stream: &tokio::net::UnixStream) -> Self {
        let pid = stream
            .peer_cred()
            .ok()
            .and_then(|credentials| credentials.pid())
            .and_then(|pid| u32::try_from(pid).ok());
        Self {
            pid,
            exe: None,
            source: pid
                .map(|_| LifecycleSource::IpcPeer)
                .unwrap_or(LifecycleSource::Unknown),
        }
    }

    /// Persist requester attribution before the daemon writes its shutdown ACK.
    pub(crate) fn record_shutdown_requested(
        self,
        paths: &crate::core::SoldrPaths,
        generation: u64,
    ) {
        use crate::daemon::lifecycle::{
            append_lifecycle_event_with, LifecycleDetails, LifecycleReason,
        };
        #[cfg(windows)]
        let peer = {
            let mut peer = self;
            if peer.exe.is_none() {
                peer.exe = peer.pid.and_then(process_executable);
            }
            peer
        };
        #[cfg(not(windows))]
        let peer = self;
        append_lifecycle_event_with(
            paths,
            "shutdown-requested",
            LifecycleDetails::requested(LifecycleReason::ExplicitStop)
                .for_target_generation(std::process::id(), generation)
                .with_peer(peer),
        );
    }

    #[cfg(windows)]
    #[allow(clippy::upper_case_acronyms, non_snake_case)]
    pub(crate) fn from_windows_named_pipe(
        pipe: &tokio::net::windows::named_pipe::NamedPipeServer,
    ) -> Self {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::raw::HANDLE;
        type BOOL = i32;
        type DWORD = u32;
        extern "system" {
            fn GetNamedPipeClientProcessId(pipe: HANDLE, client_pid: *mut DWORD) -> BOOL;
        }

        let mut pid = 0;
        // SAFETY: `pipe` owns a connected server handle for the duration of
        // the call and `pid` is a writable DWORD. Failure is best-effort
        // telemetry and intentionally becomes an unknown identity.
        if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
            return Self::unknown();
        }
        Self {
            pid: Some(pid),
            exe: None,
            source: LifecycleSource::IpcPeer,
        }
    }
}

#[cfg(unix)]
pub(crate) fn unix_peer_is_current_user(stream: &tokio::net::UnixStream) -> bool {
    stream
        .peer_cred()
        .ok()
        .is_some_and(|credentials| credentials.uid() == unsafe { libc::geteuid() })
}

#[cfg(windows)]
pub(crate) fn create_owner_only_windows_pipe(
    endpoint: &str,
    first: bool,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use std::ffi::c_void;
    use tokio::net::windows::named_pipe::ServerOptions;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            source: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    let wide: Vec<u16> = "D:P(A;;GA;;;OW)(A;;GA;;;SY)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the UTF-16 input is NUL-terminated and both output pointers are
    // valid for the duration of this call.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
        || descriptor.is_null()
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor,
        inherit: 0,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    // SAFETY: `attributes` and its descriptor remain alive through the create
    // call; Windows copies the descriptor into the new pipe object.
    let result = unsafe {
        options.create_with_security_attributes_raw(
            endpoint,
            std::ptr::addr_of_mut!(attributes).cast(),
        )
    };
    // SAFETY: ConvertStringSecurityDescriptor allocated this block with
    // LocalAlloc and LocalFree is its documented matching release.
    unsafe { LocalFree(descriptor) };
    result
}

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms, non_snake_case)]
fn process_executable(pid: u32) -> Option<String> {
    use std::os::windows::raw::HANDLE;
    type BOOL = i32;
    type DWORD = u32;
    type WCHAR = u16;
    const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
    extern "system" {
        fn OpenProcess(desired_access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn QueryFullProcessImageNameW(
            process: HANDLE,
            flags: DWORD,
            exe_name: *mut WCHAR,
            size: *mut DWORD,
        ) -> BOOL;
        fn CloseHandle(handle: HANDLE) -> BOOL;
    }

    // 32,767 UTF-16 code units is Windows' extended-path upper bound.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut path = vec![0_u16; 32_767];
    let mut len = path.len() as DWORD;
    // SAFETY: `process` is closed below; `path` has `len` writable WCHARs.
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut len) };
    unsafe { CloseHandle(process) };
    (ok != 0).then(|| String::from_utf16_lossy(&path[..len as usize]))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::core::SoldrPaths;
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

    crate::timed_test!(accepted_pipe_reports_the_os_observed_client, {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let pipe_name = format!(
                r"\\.\pipe\soldr-ipc-peer-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            );
            let server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&pipe_name)
                .expect("server");
            let _client = ClientOptions::new().open(&pipe_name).expect("client");
            server.connect().await.expect("connect");

            let peer = PeerIdentity::from_windows_named_pipe(&server);
            assert_eq!(peer.pid, Some(std::process::id()));
            assert_eq!(peer.exe, None, "the hot accept path must not resolve exe");
            assert_eq!(
                peer.source,
                crate::daemon::lifecycle::LifecycleSource::IpcPeer
            );
            let temp = tempfile::tempdir().expect("tempdir");
            let paths = SoldrPaths::with_root(temp.path().join("root"));
            peer.record_shutdown_requested(&paths, 99);
            let lifecycle =
                std::fs::read_to_string(crate::cache_lib::daemon_lifecycle_log_path(&paths))
                    .expect("lifecycle");
            let event: serde_json::Value =
                serde_json::from_str(lifecycle.trim()).expect("event json");
            let expected = std::env::current_exe()
                .expect("current exe")
                .canonicalize()
                .expect("canonical current exe");
            let observed =
                std::path::PathBuf::from(event["requester_exe"].as_str().expect("peer executable"))
                    .canonicalize()
                    .expect("canonical peer executable");
            assert_eq!(observed, expected);
        });
    });
}
