//! Historical root-local daemon endpoint derivation.
//!
//! Broker-owned daemons use private route endpoints. A daemon released before
//! that transition still holds the root-local Unix socket or Windows pipe, so
//! an upgraded client needs the exact old derivation to retire it gracefully.

use crate::core::SoldrPaths;
use std::path::PathBuf;

#[cfg(unix)]
pub(super) fn resolve(paths: &SoldrPaths) -> Result<PathBuf, String> {
    use std::hash::{Hash as _, Hasher as _};

    let preferred = crate::cache_lib::soldr_daemon_dir(paths).join("sock");
    if preferred.as_os_str().len() <= 100 {
        return Ok(preferred);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    paths.cache.hash(&mut hasher);
    let suffix = format!("{:016x}", hasher.finish());
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    Ok(tmp.join(format!("sd-{}.sock", &suffix[..12])))
}

#[cfg(windows)]
pub(super) fn resolve(paths: &SoldrPaths) -> Result<PathBuf, String> {
    let identity = windows_user_identity()?;
    Ok(PathBuf::from(format!(
        r"\\.\pipe\{}",
        compose_pipe_name(&identity, &paths.cache)
    )))
}

#[cfg(windows)]
fn compose_pipe_name(user_identity: &[u8], cache_root: &std::path::Path) -> String {
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

#[cfg(windows)]
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
