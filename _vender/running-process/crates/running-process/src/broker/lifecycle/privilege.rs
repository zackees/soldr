//! Broker startup privilege checks.
//!
//! The broker control socket is a per-user boundary. Starting it as
//! root or Windows LocalSystem would make that boundary ambiguous, so
//! the binary refuses privileged startup unless a test environment
//! explicitly opts out.

/// Environment variable that permits privileged broker startup.
///
/// This exists for controlled test fixtures only. Production launchers
/// should run the broker as the target user instead.
pub const ALLOW_PRIVILEGED_ENV: &str = "RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED";

/// Errors returned while checking broker startup privileges.
#[derive(Debug, thiserror::Error)]
pub enum PrivilegeError {
    /// The current process is running as a privileged OS identity.
    #[error(
        "running-process-broker-v1 refuses to run as {identity} by default; set {ALLOW_PRIVILEGED_ENV}=1 only for isolated test environments"
    )]
    Privileged {
        /// Privileged identity detected for the current process.
        identity: PrivilegedIdentity,
    },
    /// The platform privilege lookup failed.
    #[error("failed to determine broker process privilege: {0}")]
    PlatformLookup(String),
}

/// Privileged identities that are forbidden for the broker by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegedIdentity {
    /// Unix effective UID 0.
    UnixRoot,
    /// Windows LocalSystem account (`S-1-5-18`).
    WindowsLocalSystem,
}

impl std::fmt::Display for PrivilegedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnixRoot => f.write_str("root (effective uid 0)"),
            Self::WindowsLocalSystem => f.write_str("Windows LocalSystem (S-1-5-18)"),
        }
    }
}

/// Refuse to start the broker when the current process is privileged.
///
/// The check runs before the binary binds any socket. Set
/// [`ALLOW_PRIVILEGED_ENV`] to `1` only for isolated test environments
/// that intentionally exercise privileged startup behavior.
pub fn refuse_privileged_run() -> Result<(), PrivilegeError> {
    if allow_privileged_from_env() {
        return Ok(());
    }
    refuse_process_privilege(current_process_privilege()?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessPrivilege {
    identity: Option<PrivilegedIdentity>,
}

impl ProcessPrivilege {
    const fn unprivileged() -> Self {
        Self { identity: None }
    }

    const fn privileged(identity: PrivilegedIdentity) -> Self {
        Self {
            identity: Some(identity),
        }
    }
}

fn refuse_process_privilege(privilege: ProcessPrivilege) -> Result<(), PrivilegeError> {
    match privilege.identity {
        Some(identity) => Err(PrivilegeError::Privileged { identity }),
        None => Ok(()),
    }
}

fn allow_privileged_from_env() -> bool {
    let value = std::env::var(ALLOW_PRIVILEGED_ENV).ok();
    allow_privileged_env_value(value.as_deref())
}

fn allow_privileged_env_value(value: Option<&str>) -> bool {
    value == Some("1")
}

fn current_process_privilege() -> Result<ProcessPrivilege, PrivilegeError> {
    platform_current_process_privilege()
}

#[cfg(unix)]
fn platform_current_process_privilege() -> Result<ProcessPrivilege, PrivilegeError> {
    let euid = unsafe { libc::geteuid() };
    Ok(privilege_from_unix_euid(euid))
}

#[cfg(unix)]
fn privilege_from_unix_euid(euid: libc::uid_t) -> ProcessPrivilege {
    if euid == 0 {
        ProcessPrivilege::privileged(PrivilegedIdentity::UnixRoot)
    } else {
        ProcessPrivilege::unprivileged()
    }
}

#[cfg(windows)]
fn platform_current_process_privilege() -> Result<ProcessPrivilege, PrivilegeError> {
    let sid = windows_current_user_sid_bytes()?;
    if is_windows_local_system_sid(&sid) {
        Ok(ProcessPrivilege::privileged(
            PrivilegedIdentity::WindowsLocalSystem,
        ))
    } else {
        Ok(ProcessPrivilege::unprivileged())
    }
}

#[cfg(all(not(unix), not(windows)))]
fn platform_current_process_privilege() -> Result<ProcessPrivilege, PrivilegeError> {
    Ok(ProcessPrivilege::unprivileged())
}

#[cfg(windows)]
fn windows_current_user_sid_bytes() -> Result<Vec<u8>, PrivilegeError> {
    use std::ptr;
    use winapi::shared::winerror::ERROR_INSUFFICIENT_BUFFER;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use winapi::um::securitybaseapi::{GetLengthSid, GetTokenInformation, IsValidSid};
    use winapi::um::winnt::{TokenUser, HANDLE, TOKEN_QUERY, TOKEN_USER};

    // SAFETY: this follows the standard Windows token query pattern:
    // open the current process token, ask for the required TOKEN_USER
    // buffer size, then copy the SID bytes out while the buffer is
    // still alive.
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(PrivilegeError::PlatformLookup(format!(
                "OpenProcessToken failed (GetLastError={})",
                GetLastError()
            )));
        }
        let token = TokenHandle(token);

        let mut required_size = 0_u32;
        let ok = GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required_size);
        let last = GetLastError();
        if ok != 0 || last != ERROR_INSUFFICIENT_BUFFER {
            return Err(PrivilegeError::PlatformLookup(format!(
                "GetTokenInformation size query failed (ok={ok}, GetLastError={last})"
            )));
        }
        if required_size == 0 {
            return Err(PrivilegeError::PlatformLookup(
                "GetTokenInformation reported 0 required bytes".into(),
            ));
        }

        let mut buf = vec![0_u8; required_size as usize];
        if GetTokenInformation(
            token.0,
            TokenUser,
            buf.as_mut_ptr().cast(),
            required_size,
            &mut required_size,
        ) == 0
        {
            return Err(PrivilegeError::PlatformLookup(format!(
                "GetTokenInformation real query failed (GetLastError={})",
                GetLastError()
            )));
        }

        let token_user: *const TOKEN_USER = buf.as_ptr().cast();
        let sid = (*token_user).User.Sid;
        if sid.is_null() {
            return Err(PrivilegeError::PlatformLookup(
                "TOKEN_USER returned a null SID pointer".into(),
            ));
        }
        if IsValidSid(sid) == 0 {
            return Err(PrivilegeError::PlatformLookup(
                "IsValidSid returned false".into(),
            ));
        }

        let len = GetLengthSid(sid) as usize;
        if len == 0 || len > 1024 {
            return Err(PrivilegeError::PlatformLookup(format!(
                "GetLengthSid returned implausible length {len}"
            )));
        }
        Ok(std::slice::from_raw_parts(sid as *const u8, len).to_vec())
    }
}

#[cfg(windows)]
struct TokenHandle(winapi::um::winnt::HANDLE);

#[cfg(windows)]
impl Drop for TokenHandle {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn is_windows_local_system_sid(sid: &[u8]) -> bool {
    const LOCAL_SYSTEM_SID: &[u8] = &[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
    sid == LOCAL_SYSTEM_SID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_privileged_identity() {
        let err =
            refuse_process_privilege(ProcessPrivilege::privileged(PrivilegedIdentity::UnixRoot))
                .unwrap_err();
        assert!(matches!(
            err,
            PrivilegeError::Privileged {
                identity: PrivilegedIdentity::UnixRoot
            }
        ));
    }

    #[test]
    fn allows_unprivileged_identity() {
        refuse_process_privilege(ProcessPrivilege::unprivileged()).unwrap();
    }

    #[test]
    fn allow_env_value_requires_exact_one() {
        assert!(allow_privileged_env_value(Some("1")));
        assert!(!allow_privileged_env_value(None));
        assert!(!allow_privileged_env_value(Some("")));
        assert!(!allow_privileged_env_value(Some("true")));
        assert!(!allow_privileged_env_value(Some("yes")));
    }

    #[cfg(unix)]
    #[test]
    fn unix_root_detection_uses_effective_uid_zero() {
        assert_eq!(
            privilege_from_unix_euid(0),
            ProcessPrivilege::privileged(PrivilegedIdentity::UnixRoot)
        );
        assert_eq!(
            privilege_from_unix_euid(1000),
            ProcessPrivilege::unprivileged()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_local_system_sid_is_detected() {
        assert!(is_windows_local_system_sid(&[
            1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0
        ]));
        assert!(!is_windows_local_system_sid(&[
            1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0
        ]));
    }
}
