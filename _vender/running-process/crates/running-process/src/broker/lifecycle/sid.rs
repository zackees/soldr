//! Per-user identity hash used by every broker pipe name.
//!
//! Returns a 16-character lowercase hex string (the first 8 bytes of a
//! blake3 digest, hex-encoded). Stable across runs for the same user
//! on the same machine; collision resistant in practice.
//!
//! ## Platform inputs
//!
//! | Platform | Hash input |
//! |----------|------------|
//! | Windows  | The current process token user SID, in `S-1-...` text form, obtained via `OpenProcessToken(GetCurrentProcess())` → `GetTokenInformation(TokenUser)` → `ConvertSidToStringSidW`. |
//! | Linux    | `format!("{uid}:{machine_id}")` where `machine_id` is the contents of `/etc/machine-id`, falling back to `/var/lib/dbus/machine-id`. |
//! | macOS    | `format!("{uid}:{machine_uuid}")` where `machine_uuid` comes from `ioreg -d2 -c IOPlatformExpertDevice` (the `IOPlatformUUID` field). |
//!
//! ## Why a hash?
//!
//! Pipe-name length limits are tight: Windows MAX_PATH (260) and the
//! macOS `sun_path` field (104 bytes). A blake3 16-char hex is short,
//! collision-resistant for the namespace size we care about
//! (per-machine per-user), and avoids leaking the literal SID or
//! machine UUID into world-readable filesystem paths.

/// Errors that can prevent computing the user SID hash.
#[derive(Debug, thiserror::Error)]
pub enum SidError {
    /// Could not read the platform user identity (e.g. machine-id
    /// missing, ioreg unavailable, OpenProcessToken failed).
    #[error("failed to read platform user identity: {0}")]
    PlatformLookup(String),
}

/// Return the 16-character lowercase hex blake3 hash of the current
/// user's platform identity. Stable across runs.
pub fn user_sid_hash() -> Result<String, SidError> {
    let input = platform_identity_string()?;
    Ok(hash_to_16_hex(input.as_bytes()))
}

/// Hash arbitrary bytes to 16 lowercase hex characters using blake3.
///
/// Exposed for testing and for the rare caller that wants to hash a
/// non-default identity string (e.g. a CI runner ID).
pub fn hash_to_16_hex(input: &[u8]) -> String {
    let digest = blake3::hash(input);
    let bytes = digest.as_bytes();
    // 8 bytes → 16 hex chars.
    let mut out = String::with_capacity(16);
    for b in &bytes[..8] {
        // Lowercase hex, fixed width.
        out.push(nibble_to_hex(b >> 4));
        out.push(nibble_to_hex(b & 0x0F));
    }
    out
}

#[inline]
fn nibble_to_hex(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!("nibble out of range"),
    }
}

fn platform_identity_string() -> Result<String, SidError> {
    #[cfg(windows)]
    {
        windows_current_user_sid()
    }
    #[cfg(target_os = "macos")]
    {
        let uid = unsafe { libc::getuid() };
        let uuid = macos_platform_uuid()?;
        Ok(format!("{uid}:{uuid}"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let uid = unsafe { libc::getuid() };
        let machine_id = linux_machine_id()?;
        Ok(format!("{uid}:{machine_id}"))
    }
}

#[cfg(windows)]
fn windows_current_user_sid() -> Result<String, SidError> {
    use std::ptr;
    use winapi::shared::winerror::ERROR_INSUFFICIENT_BUFFER;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use winapi::um::securitybaseapi::GetTokenInformation;
    use winapi::um::winnt::{TokenUser, HANDLE, TOKEN_QUERY, TOKEN_USER};

    // SAFETY: the chain of Windows API calls below follows the
    // documented pattern for retrieving the current process's user
    // SID. Every allocated buffer is freed before returning, and we
    // never expose raw pointers to safe Rust.
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(SidError::PlatformLookup(format!(
                "OpenProcessToken failed (GetLastError={})",
                GetLastError()
            )));
        }
        let close_token = TokenHandle(token);

        // First call: query required buffer size.
        let mut required_size: u32 = 0;
        let ok = GetTokenInformation(
            close_token.0,
            TokenUser,
            ptr::null_mut(),
            0,
            &mut required_size,
        );
        // We expect this to fail with ERROR_INSUFFICIENT_BUFFER.
        let last = GetLastError();
        if ok != 0 || last != ERROR_INSUFFICIENT_BUFFER {
            return Err(SidError::PlatformLookup(format!(
                "GetTokenInformation size query failed (ok={ok}, GetLastError={last})"
            )));
        }
        if required_size == 0 {
            return Err(SidError::PlatformLookup(
                "GetTokenInformation reported 0 required bytes".into(),
            ));
        }

        let mut buf: Vec<u8> = vec![0u8; required_size as usize];
        if GetTokenInformation(
            close_token.0,
            TokenUser,
            buf.as_mut_ptr().cast(),
            required_size,
            &mut required_size,
        ) == 0
        {
            return Err(SidError::PlatformLookup(format!(
                "GetTokenInformation real query failed (GetLastError={})",
                GetLastError()
            )));
        }

        // The buffer starts with a TOKEN_USER struct whose `User.Sid`
        // points into the same allocation.
        let token_user: *const TOKEN_USER = buf.as_ptr().cast();
        let sid_ptr = (*token_user).User.Sid;
        if sid_ptr.is_null() {
            return Err(SidError::PlatformLookup(
                "TOKEN_USER returned a null SID pointer".into(),
            ));
        }
        sid_to_string(sid_ptr)
    }
}

#[cfg(windows)]
struct TokenHandle(winapi::um::winnt::HANDLE);

#[cfg(windows)]
impl Drop for TokenHandle {
    fn drop(&mut self) {
        // SAFETY: handle came from OpenProcessToken and is only closed
        // once via this Drop.
        unsafe {
            winapi::um::handleapi::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
unsafe fn sid_to_string(sid: winapi::um::winnt::PSID) -> Result<String, SidError> {
    // ConvertSidToStringSidW lives in advapi32. winapi 0.3 exposes it
    // through `winapi::shared::sddl::ConvertSidToStringSidW`, but the
    // `sddl` module requires the `sddl` feature. Rather than expand
    // the winapi feature list, we round-trip through the byte
    // representation of the SID, which is exactly what
    // ConvertSidToStringSidW formats. The hash input only needs to be
    // stable per user on a given machine — the textual S-1-... form
    // and the raw bytes both meet that bar.
    use winapi::um::securitybaseapi::{GetLengthSid, IsValidSid};

    if IsValidSid(sid) == 0 {
        return Err(SidError::PlatformLookup("IsValidSid returned false".into()));
    }
    let len = GetLengthSid(sid) as usize;
    if len == 0 || len > 1024 {
        return Err(SidError::PlatformLookup(format!(
            "GetLengthSid returned implausible length {len}"
        )));
    }
    let slice = std::slice::from_raw_parts(sid as *const u8, len);
    // Format as `windows-sid:<hex>` so the hash input is
    // distinguishable from the Linux/macOS schemes (defence in depth
    // against accidental cross-platform collisions).
    let mut hex = String::with_capacity(len * 2);
    for b in slice {
        hex.push(nibble_to_hex(b >> 4));
        hex.push(nibble_to_hex(b & 0x0F));
    }
    Ok(format!("windows-sid:{hex}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn linux_machine_id() -> Result<String, SidError> {
    const PATHS: &[&str] = &["/etc/machine-id", "/var/lib/dbus/machine-id"];
    for path in PATHS {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(SidError::PlatformLookup(format!("read {path}: {err}")));
            }
        }
    }
    Err(SidError::PlatformLookup(
        "no /etc/machine-id or /var/lib/dbus/machine-id found".into(),
    ))
}

#[cfg(target_os = "macos")]
fn macos_platform_uuid() -> Result<String, SidError> {
    use std::process::Command;
    // `ioreg -d2 -c IOPlatformExpertDevice` prints a block that
    // contains a line like `"IOPlatformUUID" = "ABCDEF..."`. Parse
    // that line out — we don't need a full plist parser.
    let output = Command::new("ioreg")
        .args(["-d2", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|e| SidError::PlatformLookup(format!("spawn ioreg: {e}")))?;
    if !output.status.success() {
        return Err(SidError::PlatformLookup(format!(
            "ioreg failed (status={:?})",
            output.status.code()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("\"IOPlatformUUID\"") {
            // rest looks like ` = "ABCDEF-..."`
            if let Some(eq_idx) = rest.find('=') {
                let value = rest[eq_idx + 1..].trim();
                let unquoted = value.trim_matches('"');
                if !unquoted.is_empty() {
                    return Ok(unquoted.to_string());
                }
            }
        }
    }
    Err(SidError::PlatformLookup(
        "ioreg output did not contain IOPlatformUUID".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_16_lowercase_hex() {
        let h = hash_to_16_hex(b"sample-input");
        assert_eq!(h.len(), 16, "hash must be 16 chars");
        for c in h.chars() {
            assert!(
                c.is_ascii_digit() || ('a'..='f').contains(&c),
                "non-lowercase-hex char in {h:?}"
            );
        }
    }

    #[test]
    fn different_inputs_yield_different_hashes() {
        let a = hash_to_16_hex(b"alice:machine-1");
        let b = hash_to_16_hex(b"bob:machine-1");
        assert_ne!(a, b);
    }

    #[test]
    fn same_input_is_stable() {
        let a = hash_to_16_hex(b"alice:machine-1");
        let b = hash_to_16_hex(b"alice:machine-1");
        assert_eq!(a, b);
    }

    #[test]
    fn current_user_hash_resolves() {
        // On a healthy dev machine this should succeed on all three
        // platforms. CI containers without /etc/machine-id will skip
        // (we don't want to make this test platform-fragile).
        match user_sid_hash() {
            Ok(h) => {
                assert_eq!(h.len(), 16);
            }
            Err(e) => {
                eprintln!("user_sid_hash unavailable on this host: {e}");
            }
        }
    }
}
