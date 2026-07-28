//! Launching the detached daemon process.
//!
//! Four shapes -- unix/windows x sibling-binary/via-self -- plus the Windows
//! `CreateProcessW` path that cannot use `std::process::Command` because it
//! needs `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` to inherit the spawn log and
//! nothing else (soldr#1961).
//!
//! The child environment these paths install comes from [`super::spawn_env`];
//! this module owns only how the process is started.

use std::path::Path;

use crate::core::SoldrPaths;

use super::spawn_env::*;

#[cfg(unix)]
pub(crate) fn spawn_detached_inner(daemon: &Path, args: &[String]) -> Result<(), std::io::Error> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(daemon);
    let baseline = running_process::environment::user_baseline_environment()?;
    cmd.env_clear().envs(baseline).envs(daemon_spawn_env());
    cmd.args(args).stdin(Stdio::null());
    // Diagnostic redirect: spawn the daemon's stderr/stdout to a
    // log file under the soldr cache root so a startup crash leaves
    // an artifact the wrapper can surface later. Falls back to
    // /dev/null if the path can't be created (preserves the original
    // contract).
    let log_path = SoldrPaths::new()
        .ok()
        .map(|p| p.root.join("daemon-spawn.log"));
    if let Some(path) = &log_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let stdout_file = file.try_clone().unwrap_or_else(|_| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .expect("dev/null must open")
            });
            cmd.stdout(stdout_file).stderr(file);
        } else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    unsafe {
        cmd.pre_exec(|| {
            // Detach from the parent's process group so the daemon
            // survives the wrapper's exit.
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn spawn_detached_inner(daemon: &Path, args: &[String]) -> Result<(), std::io::Error> {
    spawn_detached_windows_no_inherit(daemon, daemon, args)
}

/// Spawn the daemon via `<current-soldr-exe> daemon start --foreground`
/// rather than via the sibling `soldr-daemon` binary. Used by
/// [`try_spawn_detached`] when the sibling daemon binary is missing —
/// CI environments and slimmed-down deployments historically ship only
/// the soldr binary. Same detach semantics as
/// [`spawn_detached_inner`].
#[cfg(unix)]
pub(crate) fn spawn_detached_self_inner(
    soldr_self: &Path,
    args: &[String],
) -> Result<(), std::io::Error> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(soldr_self);
    let baseline = running_process::environment::user_baseline_environment()?;
    cmd.env_clear().envs(baseline).envs(daemon_spawn_env());
    // The process that discovers a missing daemon may itself be the
    // `zccache-soldr` hardlink. Force argv[0] back to the main CLI identity;
    // otherwise multicall dispatch treats `daemon` as a compiler path and
    // recursively enters the wrapper fallback instead of starting a daemon.
    force_daemon_via_self_cli_identity(&mut cmd);
    cmd.args(args).stdin(Stdio::null());
    let log_path = SoldrPaths::new()
        .ok()
        .map(|p| p.root.join("daemon-spawn.log"));
    if let Some(path) = &log_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let stdout_file = file.try_clone().unwrap_or_else(|_| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .expect("dev/null must open")
            });
            cmd.stdout(stdout_file).stderr(file);
        } else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn force_daemon_via_self_cli_identity(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.arg0("soldr");
}

#[cfg(windows)]
pub(crate) fn spawn_detached_self_inner(
    soldr_self: &Path,
    args: &[String],
) -> Result<(), std::io::Error> {
    spawn_detached_windows_no_inherit(soldr_self, Path::new("soldr"), args)
}

/// Open (or create) `daemon-spawn.log` for append with an **inheritable**
/// handle, so the detached daemon's stdout/stderr can be redirected into it.
///
/// soldr#1961. The Unix spawn paths already do this via `Stdio::from(File)`;
/// Windows wrote the child's output nowhere, so a daemon that died at startup
/// left no artifact at all -- while `soldr logs` advertised the file on every
/// platform. That also silenced the `eprintln!` #1902 deliberately used
/// instead of `tracing::info!` so the resolved compile concurrency would
/// survive the daemon's WARN-level subscriber.
///
/// `None` on any failure: a log we cannot open must degrade to today's
/// no-redirect behaviour, never fail the spawn. That mirrors the Unix paths
/// falling back to `Stdio::null()`.
#[cfg(windows)]
pub(crate) fn open_inheritable_spawn_log() -> Option<std::fs::File> {
    open_inheritable_spawn_log_at(&SoldrPaths::new().ok()?.root.join("daemon-spawn.log"))
}

/// [`open_inheritable_spawn_log`] with the path supplied, so the inheritable-
/// handle behaviour is testable without depending on the caller's real soldr
/// root.
#[cfg(windows)]
pub(crate) fn open_inheritable_spawn_log_at(path: &Path) -> Option<std::fs::File> {
    use std::os::windows::io::AsRawHandle;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;

    extern "system" {
        fn SetHandleInformation(
            hObject: std::os::windows::raw::HANDLE,
            dwMask: u32,
            dwFlags: u32,
        ) -> i32;
    }
    // HANDLE_FLAG_INHERIT. Required twice over: `bInheritHandles: TRUE` only
    // passes handles already marked inheritable, and every handle named in a
    // PROC_THREAD_ATTRIBUTE_HANDLE_LIST must be inheritable or
    // `CreateProcessW` fails outright with ERROR_INVALID_PARAMETER.
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    // SAFETY: `file` owns a live handle for the duration of this call and
    // beyond -- it is returned to the caller, who keeps it alive across
    // `CreateProcessW`.
    let ok = unsafe {
        SetHandleInformation(
            file.as_raw_handle(),
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(file)
}

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms)]
pub(crate) fn spawn_detached_windows_no_inherit(
    program: &Path,
    argv0: &Path,
    args: &[String],
) -> Result<(), std::io::Error> {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::raw::HANDLE;
    use std::ptr::{null, null_mut};

    #[allow(non_camel_case_types)]
    // Win32 API spelling — clippy would rename to Dword.
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(non_camel_case_types)]
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    #[allow(non_camel_case_types)]
    type LPVOID = *mut c_void;
    #[allow(non_camel_case_types)]
    type LPCVOID = *const c_void;
    #[allow(non_camel_case_types)]
    type LPCWSTR = *const u16;
    #[allow(non_camel_case_types)]
    type LPWSTR = *mut u16;
    #[allow(non_camel_case_types)]
    type WORD = u16;

    #[repr(C)]
    #[allow(non_snake_case)]
    struct STARTUPINFOW {
        cb: DWORD,
        lpReserved: LPWSTR,
        lpDesktop: LPWSTR,
        lpTitle: LPWSTR,
        dwX: DWORD,
        dwY: DWORD,
        dwXSize: DWORD,
        dwYSize: DWORD,
        dwXCountChars: DWORD,
        dwYCountChars: DWORD,
        dwFillAttribute: DWORD,
        dwFlags: DWORD,
        wShowWindow: WORD,
        cbReserved2: WORD,
        lpReserved2: *mut u8,
        hStdInput: HANDLE,
        hStdOutput: HANDLE,
        hStdError: HANDLE,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct PROCESS_INFORMATION {
        hProcess: HANDLE,
        hThread: HANDLE,
        dwProcessId: DWORD,
        dwThreadId: DWORD,
    }

    // soldr#1961: `STARTUPINFOW` plus the attribute-list pointer. Passed to
    // `CreateProcessW` with EXTENDED_STARTUPINFO_PRESENT so the child can be
    // given *exactly* the log handle and nothing else.
    #[repr(C)]
    #[allow(non_snake_case)]
    struct STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW,
        lpAttributeList: LPVOID,
    }

    extern "system" {
        fn CreateProcessW(
            lpApplicationName: LPCWSTR,
            lpCommandLine: LPWSTR,
            lpProcessAttributes: LPVOID,
            lpThreadAttributes: LPVOID,
            bInheritHandles: BOOL,
            dwCreationFlags: DWORD,
            lpEnvironment: LPCVOID,
            lpCurrentDirectory: LPCWSTR,
            lpStartupInfo: *mut STARTUPINFOW,
            lpProcessInformation: *mut PROCESS_INFORMATION,
        ) -> BOOL;
        fn CloseHandle(hObject: HANDLE) -> BOOL;
        fn InitializeProcThreadAttributeList(
            lpAttributeList: LPVOID,
            dwAttributeCount: DWORD,
            dwFlags: DWORD,
            lpSize: *mut usize,
        ) -> BOOL;
        fn UpdateProcThreadAttribute(
            lpAttributeList: LPVOID,
            dwFlags: DWORD,
            Attribute: usize,
            lpValue: LPVOID,
            cbSize: usize,
            lpPreviousValue: LPVOID,
            lpReturnSize: *mut usize,
        ) -> BOOL;
        fn DeleteProcThreadAttributeList(lpAttributeList: LPVOID);
    }

    const STARTF_USESTDHANDLES: DWORD = 0x0000_0100;
    const EXTENDED_STARTUPINFO_PRESENT: DWORD = 0x0008_0000;
    const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;

    // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW |
    // CREATE_UNICODE_ENVIRONMENT.
    const FLAGS: DWORD = 0x0000_0200 | 0x0000_0008 | 0x0800_0000 | 0x0000_0400;

    let application: Vec<u16> = program.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut command_line = build_windows_command_line(argv0, args);
    let environment = merged_windows_environment_block()?;
    // SAFETY: STARTUPINFOW and PROCESS_INFORMATION are plain Win32 POD
    // structs. Zero initialization is the documented baseline before setting
    // STARTUPINFOW.cb and passing both structs to CreateProcessW.
    let mut startup_ex: STARTUPINFOEXW = unsafe { zeroed() };
    startup_ex.StartupInfo.cb = size_of::<STARTUPINFOW>() as DWORD;
    let mut process_info: PROCESS_INFORMATION = unsafe { zeroed() };

    // soldr#1961: redirect the child's stdout/stderr into `daemon-spawn.log`
    // so a startup crash leaves an artifact, matching the Unix paths.
    //
    // `bInheritHandles: FALSE` was load-bearing -- it kept the child from
    // inheriting Cargo/test pipe handles from the wrapper. So this does not
    // flip it blindly: `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` names the *only*
    // handle the child may inherit, and `TRUE` then applies to that list
    // alone. The original guarantee is preserved by construction rather than
    // by hoping no other inheritable handles are open.
    //
    // Every failure below falls through to the original no-redirect spawn.
    let log_file = open_inheritable_spawn_log();
    let mut attribute_buffer: Vec<u8> = Vec::new();
    let mut handle_list: [HANDLE; 1] = [null_mut::<c_void>() as HANDLE];
    let mut flags = FLAGS;
    let mut inherit_handles: BOOL = 0;

    if let Some(ref file) = log_file {
        use std::os::windows::io::AsRawHandle;
        let log_handle = file.as_raw_handle();
        handle_list[0] = log_handle;

        let mut size: usize = 0;
        // SAFETY: the documented two-call sizing protocol -- the first call is
        // expected to fail with ERROR_INSUFFICIENT_BUFFER and write `size`.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
        if size > 0 {
            attribute_buffer.resize(size, 0);
            let list: LPVOID = attribute_buffer.as_mut_ptr().cast();
            // SAFETY: `list` points at a `size`-byte allocation that outlives
            // the CreateProcessW call below, and `handle_list` likewise -- the
            // attribute list stores the pointer rather than copying.
            let initialized = unsafe {
                InitializeProcThreadAttributeList(list, 1, 0, &mut size) != 0
                    && UpdateProcThreadAttribute(
                        list,
                        0,
                        PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                        handle_list.as_mut_ptr().cast(),
                        size_of::<HANDLE>(),
                        null_mut(),
                        null_mut(),
                    ) != 0
            };
            if initialized {
                startup_ex.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as DWORD;
                startup_ex.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
                // stdin stays null: a detached daemon has no console to read
                // from, and leaving it unset would inherit nothing anyway.
                startup_ex.StartupInfo.hStdOutput = log_handle;
                startup_ex.StartupInfo.hStdError = log_handle;
                startup_ex.lpAttributeList = list;
                flags |= EXTENDED_STARTUPINFO_PRESENT;
                inherit_handles = 1;
            } else {
                // SAFETY: only reached when InitializeProcThreadAttributeList
                // succeeded and UpdateProcThreadAttribute failed; the list is
                // initialized and must be released.
                unsafe { DeleteProcThreadAttributeList(list) };
                attribute_buffer.clear();
            }
        }
    }

    // SAFETY: application and command_line are null-terminated UTF-16 buffers
    // that live for the duration of the call. Remaining optional pointer
    // parameters are null by design. `inherit_handles` is TRUE only alongside
    // an explicit single-entry handle list (see above), so the child still
    // cannot inherit Cargo/test pipe handles.
    let ok = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            null_mut(),
            null_mut(),
            inherit_handles,
            flags,
            environment.as_ptr().cast(),
            null(),
            (&mut startup_ex as *mut STARTUPINFOEXW).cast(),
            &mut process_info,
        )
    };
    if !attribute_buffer.is_empty() {
        // SAFETY: initialized above and not used again after this point.
        unsafe { DeleteProcThreadAttributeList(attribute_buffer.as_mut_ptr().cast()) };
    }
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: CreateProcessW initialized these handles on success; this
    // process does not need to retain either handle after the detached spawn.
    unsafe {
        CloseHandle(process_info.hThread);
        CloseHandle(process_info.hProcess);
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn build_windows_command_line(program: &Path, args: &[String]) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let mut out = Vec::new();
    out.push('"' as u16);
    out.extend(program.as_os_str().encode_wide());
    out.push('"' as u16);
    for arg in args {
        out.push(' ' as u16);
        out.extend(OsStr::new(arg).encode_wide());
    }
    out.push(0);
    out
}
