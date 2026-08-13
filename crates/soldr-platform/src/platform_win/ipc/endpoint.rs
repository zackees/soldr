//! Windows endpoint naming.

use std::path::{Path, PathBuf};

/// The `sun_path` capacity concept does not apply to named pipes; the
/// caller only consults this on the Unix branch.
pub fn sun_path_capacity() -> usize {
    0
}

/// The caller only consults this on the Unix branch.
pub fn machine_runtime_dir() -> PathBuf {
    PathBuf::new()
}

/// Named-pipe paths carry no filesystem-magic constraints; the caller
/// only consults this on the Unix branch.
pub fn path_is_on_non_bindable_filesystem(_path: &Path) -> bool {
    false
}

/// The raw bytes of a socket path (Unix: the exact `OsStr` bytes
/// that feed the socket name; Windows: the lossy UTF-8 form — the
/// caller only consults this on the Unix branch).
pub fn socket_path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().to_string_lossy().as_bytes().to_vec()
}

/// Whether a socket path of `capacity`-shaped budgets fits; named pipes
/// use their own 256-character ceiling, so this answers the Windows
/// length question.
pub fn socket_path_fits(path: &Path, capacity: usize) -> bool {
    path.as_os_str().len() < capacity
}

/// Historical root-local daemon pipe name: `\\.\pipe\soldr-daemon-{user}-{cache}`.
///
/// Daemons released before broker-owned route endpoints held the
/// root-local Unix socket / Windows pipe, so an upgraded client needs the
/// exact old derivation to retire that daemon gracefully. The user
/// identity is the current process token's SID.
pub fn legacy_daemon_endpoint(cache_root: &Path) -> Result<String, String> {
    let identity = windows_user_identity()?;
    Ok(format!(
        r"\\.\pipe\{}",
        compose_pipe_name(&identity, cache_root)
    ))
}

fn compose_pipe_name(user_identity: &[u8], cache_root: &Path) -> String {
    use std::hash::{Hash as _, Hasher as _};

    fn short_hash(feed: impl FnOnce(&mut std::collections::hash_map::DefaultHasher)) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        feed(&mut hasher);
        format!("{:016x}", hasher.finish())[..12].to_string()
    }

    let identity = short_hash(|hasher| user_identity.hash(hasher));
    let cache = short_hash(|hasher| cache_root.hash(hasher));
    format!("soldr-daemon-{identity}-{cache}")
}

#[allow(clippy::upper_case_acronyms)]
fn windows_user_identity() -> Result<Vec<u8>, String> {
    type DWORD = u32;
    type BOOL = i32;
    type HANDLE = *mut std::ffi::c_void;

    const TOKEN_QUERY: DWORD = 0x0008;
    const TOKEN_USER_CLASS: i32 = 1;

    extern "system" {
        fn GetCurrentProcess() -> HANDLE;
        fn OpenProcessToken(process: HANDLE, desired_access: DWORD, token: *mut HANDLE) -> BOOL;
        fn GetTokenInformation(
            token: HANDLE,
            class: i32,
            info: *mut std::ffi::c_void,
            info_len: DWORD,
            return_len: *mut DWORD,
        ) -> BOOL;
        fn GetLengthSid(sid: *const std::ffi::c_void) -> DWORD;
        fn IsValidSid(sid: *const std::ffi::c_void) -> BOOL;
        fn CloseHandle(handle: HANDLE) -> BOOL;
    }

    struct TokenHandle(HANDLE);
    impl Drop for TokenHandle {
        fn drop(&mut self) {
            // SAFETY: this handle was returned by OpenProcessToken and remains
            // owned by TokenHandle until this destructor runs.
            unsafe { CloseHandle(self.0) };
        }
    }

    // SAFETY: GetCurrentProcess returns the current pseudo-handle and `token`
    // is a valid writable out-pointer for OpenProcessToken.
    let token = unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(format!(
                "cannot derive legacy daemon endpoint: OpenProcessToken failed ({})",
                std::io::Error::last_os_error()
            ));
        }
        TokenHandle(token)
    };

    let mut needed = 0;
    // SAFETY: a null information buffer with length zero is the documented
    // size-query form; `needed` is a valid writable DWORD.
    unsafe {
        GetTokenInformation(
            token.0,
            TOKEN_USER_CLASS,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        return Err("cannot derive legacy daemon endpoint: TokenUser size is zero".into());
    }

    let mut buffer = vec![0_u8; needed as usize];
    // SAFETY: `buffer` is writable for `needed` bytes and `needed` remains a
    // valid writable DWORD for the returned byte count.
    if unsafe {
        GetTokenInformation(
            token.0,
            TOKEN_USER_CLASS,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(format!(
            "cannot derive legacy daemon endpoint: TokenUser query failed ({})",
            std::io::Error::last_os_error()
        ));
    }
    if buffer.len() < std::mem::size_of::<*const std::ffi::c_void>() {
        return Err("cannot derive legacy daemon endpoint: TokenUser buffer too small".into());
    }
    // SAFETY: TOKEN_USER starts with a SID pointer. The size check above makes
    // this unaligned pointer read stay within the returned buffer.
    let sid = unsafe {
        buffer
            .as_ptr()
            .cast::<*const std::ffi::c_void>()
            .read_unaligned()
    };
    // SAFETY: a non-null SID pointer returned in TOKEN_USER remains valid while
    // `buffer` is alive.
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err("cannot derive legacy daemon endpoint: invalid TokenUser SID".into());
    }
    // SAFETY: IsValidSid accepted this pointer immediately above.
    let sid_len = unsafe { GetLengthSid(sid) } as usize;
    if sid_len == 0 {
        return Err("cannot derive legacy daemon endpoint: TokenUser SID is empty".into());
    }
    // SAFETY: GetLengthSid supplies the extent of the valid SID allocation,
    // whose lifetime is tied to `buffer` through this copy.
    Ok(unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), sid_len) }.to_vec())
}
