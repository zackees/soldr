//! Process environment baselines.
//!
//! Windows exposes the logged-in user's machine + user environment through
//! `CreateEnvironmentBlock`. Unix has no OS API that reconstructs a login
//! environment, so the baseline is rebuilt from the user's identity:
//! `getpwuid_r` supplies `USER`/`LOGNAME`/`HOME`/`SHELL`, `PATH` gets the
//! platform's login default, and locale/timezone/tmpdir variables are
//! carried over from the current process when present.

#[cfg(windows)]
use std::ffi::c_void;
use std::ffi::OsString;
use std::io;

/// Return the logged-in user's baseline environment.
///
/// On Windows this is freshly constructed from machine and user settings via
/// `CreateEnvironmentBlock` and therefore excludes variables that exist only
/// in the current process. On Unix it is reconstructed from the user's
/// identity (`getpwuid_r`): `USER`, `LOGNAME`, `HOME`, `SHELL`, a platform
/// default `PATH`, plus locale (`LANG`, `LC_*`), `TZ`, and `TMPDIR` carried
/// over from the current process when set. Non-Windows targets without a
/// resolvable passwd entry fall back to the current process environment.
pub fn user_baseline_environment() -> io::Result<Vec<(OsString, OsString)>> {
    #[cfg(windows)]
    {
        let block = user_baseline_environment_block()?;
        Ok(parse_windows_environment_block(&block))
    }
    #[cfg(unix)]
    {
        Ok(unix_login_baseline_environment().unwrap_or_else(|| std::env::vars_os().collect()))
    }
    #[cfg(not(any(windows, unix)))]
    {
        Ok(std::env::vars_os().collect())
    }
}

/// The `PATH` a fresh login would start from. Matches `/etc/paths` order on
/// macOS and the customary `login(1)` default elsewhere.
#[cfg(unix)]
const UNIX_LOGIN_DEFAULT_PATH: &str = if cfg!(target_os = "macos") {
    "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
} else {
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
};

/// Build a clean login environment from the user's identity instead of the
/// current process environment. Returns `None` when the passwd entry cannot
/// be resolved (e.g. UID absent from NSS) so the caller can fall back.
#[cfg(unix)]
fn unix_login_baseline_environment() -> Option<Vec<(OsString, OsString)>> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStringExt;

    let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // sysconf(_SC_GETPW_R_SIZE_MAX) is allowed to return -1 ("no limit");
    // 1 KiB covers real-world passwd entries and getpwuid_r reports ERANGE
    // if it does not, in which case we grow and retry.
    let mut buf = vec![0u8; 1024];
    loop {
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut passwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        break;
    }

    let field = |ptr: *const libc::c_char| -> Option<OsString> {
        if ptr.is_null() {
            return None;
        }
        let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
        (!bytes.is_empty()).then(|| OsString::from_vec(bytes.to_vec()))
    };
    let name = field(passwd.pw_name)?;
    let home = field(passwd.pw_dir)?;

    let mut env: Vec<(OsString, OsString)> = vec![
        (OsString::from("USER"), name.clone()),
        (OsString::from("LOGNAME"), name),
        (OsString::from("HOME"), home),
        (
            OsString::from("PATH"),
            OsString::from(UNIX_LOGIN_DEFAULT_PATH),
        ),
    ];
    if let Some(shell) = field(passwd.pw_shell) {
        env.push((OsString::from("SHELL"), shell));
    }
    // Locale, timezone, and per-user tmpdir describe the session rather
    // than the parent process; carry them over when present so children
    // keep rendering text and resolving paths the way the user expects.
    for (key, value) in std::env::vars_os() {
        let carry = key == "LANG" || key == "TZ" || key == "TMPDIR" || {
            key.to_str().is_some_and(|k| k.starts_with("LC_"))
        };
        if carry {
            env.push((key, value));
        }
    }
    Some(env)
}

/// Return a CreateProcessW-compatible Unicode user environment block.
///
/// The returned buffer is sorted and double-NUL terminated by Windows. It is
/// useful to callers that own a manual `CreateProcessW` path.
#[cfg(windows)]
pub fn user_baseline_environment_block() -> io::Result<Vec<u16>> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY};
    use windows_sys::Win32::System::Environment::{
        CreateEnvironmentBlock, DestroyEnvironmentBlock,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // CreateEnvironmentBlock documents that the token needs TOKEN_QUERY,
    // TOKEN_DUPLICATE, and TOKEN_IMPERSONATE. With TOKEN_QUERY alone the
    // call still *succeeds* but silently omits the per-user dynamic
    // variables (USERNAME, USERDOMAIN) — downstream consumers that key
    // behavior on USERNAME (e.g. soldr's daemon pipe name) then diverge
    // from processes holding the real login environment.
    let mut token = std::ptr::null_mut();
    let opened = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
            &mut token,
        )
    };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut raw_block: *mut c_void = std::ptr::null_mut();
    let created = unsafe { CreateEnvironmentBlock(&mut raw_block, token, 0) };
    let create_error = if created == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        CloseHandle(token);
    }
    if let Some(error) = create_error {
        return Err(error);
    }

    let copied = unsafe { copy_windows_environment_block(raw_block.cast::<u16>()) };
    unsafe {
        DestroyEnvironmentBlock(raw_block);
    }
    Ok(copied)
}

#[cfg(windows)]
unsafe fn copy_windows_environment_block(cursor: *const u16) -> Vec<u16> {
    let mut len = 0usize;
    loop {
        if *cursor.add(len) == 0 && *cursor.add(len + 1) == 0 {
            len += 2;
            break;
        }
        len += 1;
    }
    std::slice::from_raw_parts(cursor, len).to_vec()
}

#[cfg(windows)]
fn parse_windows_environment_block(block: &[u16]) -> Vec<(OsString, OsString)> {
    use std::os::windows::ffi::OsStringExt;

    let mut env = Vec::new();
    let mut offset = 0usize;
    while offset < block.len() && block[offset] != 0 {
        let Some(relative_end) = block[offset..].iter().position(|value| *value == 0) else {
            break;
        };
        let end = offset + relative_end;
        let entry = &block[offset..end];
        // Drive-current-directory pseudo variables have the shape
        // `=C:=C:\path`; skip index zero so their second '=' is the
        // key/value separator.
        if let Some(separator) = entry
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, value)| (*value == b'=' as u16).then_some(index))
        {
            let key = OsString::from_wide(&entry[..separator]);
            let value = OsString::from_wide(&entry[separator + 1..]);
            env.push((key, value));
        }
        offset = end + 1;
    }
    env
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    #[test]
    fn login_baseline_contains_identity_and_default_path() {
        let env = user_baseline_environment().unwrap();
        let get = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let user = get("USER").expect("baseline must contain USER");
        assert!(!user.is_empty());
        assert_eq!(get("LOGNAME").as_ref(), Some(&user));
        let home = get("HOME").expect("baseline must contain HOME");
        assert!(!home.is_empty());
        let path = get("PATH").expect("baseline must contain PATH");
        assert!(!path.is_empty());
    }

    #[test]
    fn login_baseline_does_not_leak_arbitrary_process_vars() {
        // A variable that only exists in this process must not survive
        // into the login baseline (that's what Inherit is for).
        std::env::set_var("RUNNING_PROCESS_BASELINE_CANARY", "1");
        let env = unix_login_baseline_environment().expect("test user must have a passwd entry");
        assert!(
            !env.iter()
                .any(|(key, _)| key == "RUNNING_PROCESS_BASELINE_CANARY"),
            "process-local variables must not leak into the login baseline"
        );
        std::env::remove_var("RUNNING_PROCESS_BASELINE_CANARY");
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[test]
    fn parser_preserves_drive_current_directory_entries() {
        let block: Vec<u16> = OsStr::new("=C:=C:\\work")
            .encode_wide()
            .chain(std::iter::once(0))
            .chain(OsStr::new("Path=C:\\Windows").encode_wide())
            .chain(std::iter::once(0))
            .chain(std::iter::once(0))
            .collect();
        assert_eq!(
            parse_windows_environment_block(&block),
            vec![
                (OsString::from("=C:"), OsString::from("C:\\work")),
                (OsString::from("Path"), OsString::from("C:\\Windows")),
            ]
        );
    }

    #[test]
    fn live_user_baseline_is_double_nul_terminated() {
        let block = user_baseline_environment_block().unwrap();
        assert!(block.len() >= 2);
        assert_eq!(&block[block.len() - 2..], &[0, 0]);
    }

    /// Regression: with TOKEN_QUERY-only access, CreateEnvironmentBlock
    /// succeeds but silently drops the per-user dynamic variables. The
    /// baseline must contain USERNAME (and it must match the live value
    /// when the current process has one).
    #[test]
    fn live_user_baseline_contains_username() {
        let env = user_baseline_environment().unwrap();
        let username = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("USERNAME"))
            .map(|(_, value)| value.clone());
        let username = username.expect("baseline environment must contain USERNAME");
        assert!(!username.is_empty(), "USERNAME must be non-empty");
        if let Ok(live) = std::env::var("USERNAME") {
            assert_eq!(
                username.to_string_lossy(),
                live,
                "baseline USERNAME must match the live login USERNAME"
            );
        }
    }
}
