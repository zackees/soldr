//! Windows peer surface: the owner-only named-pipe server used by the
//! private control endpoint, plus peer-process observation.

use std::io;

/// The accepted Windows named-pipe server, held as the concrete tokio
/// type inside the platform tree. The daemon drives it through the
/// facade functions below and the generic async frame codec.
pub type PipeServer = tokio::net::windows::named_pipe::NamedPipeServer;

/// Create the owner-only named-pipe server for the private control
/// endpoint. `first` marks the first pool instance (Windows then fails
/// a second create under the same name while this instance is not
/// connected). Owner+SYSTEM access is enforced through a security
/// descriptor so other users cannot dial the daemon control endpoint.
pub fn create_owner_only_windows_pipe(endpoint: &str, first: bool) -> io::Result<PipeServer> {
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
        return Err(io::Error::last_os_error());
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

/// Await a client connection on the server instance. The daemon races
/// this against its shutdown signal so a retiring daemon drops the
/// instance without accepting.
pub async fn pipe_server_connect(server: &mut PipeServer) -> io::Result<()> {
    server.connect().await
}

/// Observe the client process id at the other end of a connected pipe
/// (`GetNamedPipeClientProcessId`). Best-effort telemetry: a failure
/// yields `None` and the peer is reported unknown.
#[allow(clippy::upper_case_acronyms, non_snake_case)]
pub fn peer_identity_of_pipe_server(server: &mut PipeServer) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::raw::HANDLE;
    type BOOL = i32;
    type DWORD = u32;
    extern "system" {
        fn GetNamedPipeClientProcessId(pipe: HANDLE, client_pid: *mut DWORD) -> BOOL;
    }

    let mut pid = 0;
    // SAFETY: `server` owns a connected server handle for the duration of
    // the call and `pid` is a writable DWORD. Failure is best-effort
    // telemetry and intentionally becomes an unknown identity.
    if unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &mut pid) } == 0 {
        return None;
    }
    Some(pid)
}

/// Resolve the executable path of `pid`, used to attribute a shutdown
/// request to the requesting process. `None` when the process cannot be
/// opened or its image name queried (already exited, access denied).
#[allow(clippy::upper_case_acronyms, non_snake_case)]
pub fn process_executable(pid: u32) -> Option<String> {
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
