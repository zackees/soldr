//! #539 — read the live command line of any LaunchedProcessTree PID
//! without admin privileges.
//!
//! Cross-platform dispatcher that calls into a per-OS no-admin primitive:
//!
//! - **Windows**: `NtQueryInformationProcess(ProcessCommandLineInformation=60)`.
//!   Slice 3 of #539. Works for any PID the calling process has
//!   `PROCESS_QUERY_LIMITED_INFORMATION` on, which is always true for
//!   descendants of a process we spawned into our own Job Object on the
//!   non-elevated default integrity level.
//! - **Linux**: `/proc/<pid>/cmdline` — landing in slice 6 of #539.
//! - **macOS**: `sysctl(KERN_PROCARGS2)` — landing in slice 8 of #539.
//!
//! Linux and macOS branches return [`std::io::ErrorKind::Unsupported`]
//! until their respective slices land; the API surface is stable now so
//! consumers (e.g. clud) can wire to it once.

/// Read the live command line of `pid` using the negotiated no-admin
/// per-OS primitive for the `LaunchedProcessTree` scope.
///
/// Returns the command line as a UTF-8 (potentially lossy on Windows
/// where the source is UTF-16) `String`, or an `io::Error` if the PID
/// cannot be opened, has already exited, or the kernel rejected the
/// query.
///
/// On platforms where the backend hasn't shipped yet
/// (`TraceScope::LaunchedProcessTree` cmdline backend for that OS is
/// still `Unavailable`), returns `ErrorKind::Unsupported` with a reason
/// that names the future slice. This lets downstream callers code
/// against the stable surface today.
pub fn read_process_cmdline(pid: u32) -> std::io::Result<String> {
    #[cfg(target_os = "windows")]
    {
        windows_impl::read_process_cmdline(pid)
    }
    #[cfg(target_os = "linux")]
    {
        linux_impl::read_process_cmdline(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_impl::read_process_cmdline(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = pid;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "#539: no LaunchedProcessTree cmdline backend planned for this OS",
        ))
    }
}

#[cfg(target_os = "macos")]
mod macos_impl {
    //! macOS `sysctl(KERN_PROCARGS2)` implementation. Returns the
    //! actual argv the kernel handed to `execve`, fully no-admin for
    //! processes the calling user owns.
    //!
    //! Layout of the returned buffer (per `sys/sysctl.h` +
    //! `bsd/kern/kern_sysctl.c` in xnu):
    //!
    //! ```text
    //! [ argc (i32, host endianness) ]
    //! [ exec_path (NUL-terminated UTF-8 string) ]
    //! [ NUL padding to align to ptr-boundary ]
    //! [ argv[0] (NUL-terminated) ]
    //! [ argv[1] ... argv[argc-1] (each NUL-terminated) ]
    //! [ envp[0] ... envp[N] (NUL-terminated; ignored here) ]
    //! ```

    const CTL_KERN: libc::c_int = 1;
    const KERN_PROCARGS2: libc::c_int = 49;

    pub(super) fn read_process_cmdline(pid: u32) -> std::io::Result<String> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the kernel scheduler — not queryable",
            ));
        }
        let mut name: [libc::c_int; 3] = [CTL_KERN, KERN_PROCARGS2, pid as libc::c_int];
        // Size probe: pass null buf to learn the required length.
        let mut len: libc::size_t = 0;
        let r = unsafe {
            libc::sysctl(
                name.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if len < std::mem::size_of::<i32>() {
            return Err(std::io::Error::other(format!(
                "KERN_PROCARGS2 returned size={len}, smaller than argc header",
            )));
        }

        let mut buf = vec![0u8; len];
        let r = unsafe {
            libc::sysctl(
                name.as_mut_ptr(),
                3,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        buf.truncate(len);
        parse_procargs2(&buf)
    }

    fn parse_procargs2(buf: &[u8]) -> std::io::Result<String> {
        if buf.len() < std::mem::size_of::<i32>() {
            return Ok(String::new());
        }
        let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if argc <= 0 {
            return Ok(String::new());
        }
        let mut cursor = std::mem::size_of::<i32>();
        // Skip exec_path: bytes until first NUL.
        while cursor < buf.len() && buf[cursor] != 0 {
            cursor += 1;
        }
        // Skip the run of NUL padding the kernel inserts to align argv
        // start to a pointer boundary.
        while cursor < buf.len() && buf[cursor] == 0 {
            cursor += 1;
        }
        // Read exactly argc argv strings, joining with spaces — mirrors
        // the Windows NtQueryInformationProcess and Linux
        // /proc/<pid>/cmdline conventions.
        let mut argv: Vec<String> = Vec::with_capacity(argc as usize);
        for _ in 0..argc {
            if cursor >= buf.len() {
                break;
            }
            let start = cursor;
            while cursor < buf.len() && buf[cursor] != 0 {
                cursor += 1;
            }
            argv.push(String::from_utf8_lossy(&buf[start..cursor]).into_owned());
            // Skip the NUL terminator.
            cursor = cursor.saturating_add(1);
        }
        Ok(argv.join(" "))
    }

    #[cfg(test)]
    mod tests {
        use super::parse_procargs2;

        /// Build a KERN_PROCARGS2 buffer for argv = [exec, args...].
        fn build_procargs2(exec_path: &str, argv: &[&str]) -> Vec<u8> {
            let mut buf = Vec::new();
            let argc = argv.len() as i32;
            buf.extend_from_slice(&argc.to_ne_bytes());
            buf.extend_from_slice(exec_path.as_bytes());
            buf.push(0);
            // Pad to a pointer boundary with extra NULs (kernel does
            // this — exercise the skip-padding path in the parser).
            while buf.len() % 8 != 0 {
                buf.push(0);
            }
            for arg in argv {
                buf.extend_from_slice(arg.as_bytes());
                buf.push(0);
            }
            // Trailing envp would go here; we don't add any.
            buf
        }

        #[test]
        fn parses_argv_skipping_exec_path_and_padding() {
            let buf = build_procargs2("/usr/bin/myprog", &["myprog", "--flag", "value with space"]);
            let out = parse_procargs2(&buf).expect("parse");
            assert_eq!(out, "myprog --flag value with space");
        }

        #[test]
        fn empty_argv_yields_empty_string() {
            let buf = build_procargs2("/usr/bin/noop", &[]);
            let out = parse_procargs2(&buf).expect("parse");
            assert_eq!(out, "");
        }

        #[test]
        fn argc_zero_short_circuits() {
            let mut buf = 0i32.to_ne_bytes().to_vec();
            buf.extend_from_slice(b"/usr/bin/noop\0");
            let out = parse_procargs2(&buf).expect("parse");
            assert_eq!(out, "");
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    //! Linux `/proc/<pid>/cmdline` implementation. The kernel writes
    //! argv as NUL-separated UTF-8 (typically — argv is opaque bytes,
    //! we lossy-decode), with a trailing NUL.

    pub(super) fn read_process_cmdline(pid: u32) -> std::io::Result<String> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the kernel scheduler — not queryable",
            ));
        }
        let path = format!("/proc/{pid}/cmdline");
        let bytes = std::fs::read(&path)?;
        // `/proc/<pid>/cmdline` is empty for kernel threads — return
        // empty string rather than synthesizing fake separators.
        if bytes.is_empty() {
            return Ok(String::new());
        }
        // Drop the trailing NUL terminator if present, then turn
        // remaining NUL separators into spaces so the result reads as
        // a single shell-style command line (same convention as
        // Windows NtQueryInformationProcess and macOS KERN_PROCARGS2,
        // both of which return one logical command line per PID).
        let mut trimmed = bytes.as_slice();
        if trimmed.last() == Some(&0) {
            trimmed = &trimmed[..trimmed.len() - 1];
        }
        let joined: Vec<u8> = trimmed
            .iter()
            .map(|b| if *b == 0 { b' ' } else { *b })
            .collect();
        Ok(String::from_utf8_lossy(&joined).into_owned())
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    //! Windows `NtQueryInformationProcess(ProcessCommandLineInformation)`
    //! implementation. The Info class is undocumented but stable on
    //! Win8.1+ — empirically validated in clud#468 t03.

    use std::ffi::c_void;

    /// `ProcessCommandLineInformation` from `ntddk.h` — info class 60.
    /// Stable since Windows 8.1. Returns a `UNICODE_STRING` header
    /// followed by the inline wide-character cmdline bytes.
    const PROCESS_COMMAND_LINE_INFORMATION: i32 = 60;

    /// `STATUS_INFO_LENGTH_MISMATCH` (0xC0000004) — expected on the
    /// initial size-probe call.
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004u32 as i32;

    /// `STATUS_SUCCESS` (0).
    const STATUS_SUCCESS: i32 = 0;

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryInformationProcess(
            process_handle: *mut c_void,
            process_information_class: i32,
            process_information: *mut c_void,
            process_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    pub(super) fn read_process_cmdline(pid: u32) -> std::io::Result<String> {
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;

        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the system idle process — not queryable",
            ));
        }

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        let result = query_cmdline(handle as *mut c_void);
        unsafe { CloseHandle(handle) };
        result
    }

    fn query_cmdline(handle: *mut c_void) -> std::io::Result<String> {
        // Size probe: pass a zero-length buffer; expect
        // STATUS_INFO_LENGTH_MISMATCH and the required size in
        // `needed`.
        let mut needed: u32 = 0;
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                PROCESS_COMMAND_LINE_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if status != STATUS_INFO_LENGTH_MISMATCH && status != STATUS_SUCCESS {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess size probe returned status=0x{:08x}",
                status as u32,
            )));
        }
        if needed < std::mem::size_of::<UnicodeString>() as u32 {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess returned needed={needed}, smaller than UNICODE_STRING header",
            )));
        }

        let mut buf = vec![0u8; needed as usize];
        let mut returned: u32 = 0;
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                PROCESS_COMMAND_LINE_INFORMATION,
                buf.as_mut_ptr() as *mut c_void,
                needed,
                &mut returned,
            )
        };
        if status != STATUS_SUCCESS {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess returned status=0x{:08x}",
                status as u32,
            )));
        }

        // The buffer begins with a UNICODE_STRING whose `buffer` field
        // points into the same allocation, immediately past the header.
        // We cannot dereference `us.buffer` directly across the FFI
        // boundary on systems that may relocate it; instead, compute the
        // header size and read inline.
        let us = unsafe { std::ptr::read(buf.as_ptr() as *const UnicodeString) };
        let len_bytes = us.length as usize;
        if len_bytes == 0 {
            return Ok(String::new());
        }
        // The string is wide-char (UTF-16 LE) and located just after the
        // UNICODE_STRING header. The kernel writes `buffer` as a pointer
        // into our supplied allocation, but the safest portable parse is
        // to read the chars from header_size..header_size+len_bytes in
        // our own buffer.
        let header_size = std::mem::size_of::<UnicodeString>();
        if header_size + len_bytes > buf.len() {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationProcess wrote less than {} bytes for cmdline (returned={returned}, len={len_bytes})",
                header_size + len_bytes,
            )));
        }
        let wide_slice: &[u16] = unsafe {
            std::slice::from_raw_parts(buf[header_size..].as_ptr() as *const u16, len_bytes / 2)
        };
        Ok(String::from_utf16_lossy(wide_slice))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    #[test]
    fn read_cmdline_for_pid_zero_returns_invalid_input() {
        // PID 0 is the system idle process on Windows / kernel scheduler
        // on Linux + macOS — not openable from user-mode on any of them,
        // so all three backends reject it up front before touching FFI /
        // FS.
        let err = read_process_cmdline(0).expect_err("pid 0 should be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unix_read_cmdline_round_trips_known_args_from_spawned_child() {
        use crate::observer::ObserverConfig;
        use crate::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
        use std::time::Duration;

        // Long-lived `sleep 30` (available on both Linux and macOS as a
        // POSIX standard utility) with a distinctive argv: read it
        // back via the per-OS no-admin primitive while the child is
        // still alive.
        let cfg = ProcessConfig {
            command: CommandSpec::Argv(vec!["sleep".into(), "30".into()]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
        };
        let (process, _sub) = NativeProcess::with_observer(cfg, ObserverConfig::lifecycle());
        process.start().expect("spawn sleep");
        let pid = process.pid().expect("pid");
        std::thread::sleep(Duration::from_millis(100));

        let cmdline = read_process_cmdline(pid).expect("read cmdline");
        process.kill().ok();
        process.close().ok();

        assert!(
            cmdline.contains("sleep"),
            "expected 'sleep' in cmdline, got: {cmdline:?}"
        );
        assert!(
            cmdline.contains("30"),
            "expected '30' (the sleep duration) in cmdline, got: {cmdline:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_read_cmdline_for_nonexistent_pid_returns_not_found() {
        let err = read_process_cmdline(0x7FFF_FFFE).expect_err("nonexistent pid");
        // `/proc/<missing>/cmdline` open fails with ENOENT.
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_read_cmdline_for_nonexistent_pid_returns_io_error() {
        // sysctl with a missing pid returns ESRCH; we surface it
        // verbatim as the os_error code. Don't pin the exact errno
        // because newer xnu builds occasionally remap it to EINVAL
        // for hardened tasks; just assert an os_error came through.
        let err = read_process_cmdline(0x7FFF_FFFE).expect_err("nonexistent pid");
        assert!(
            err.raw_os_error().is_some(),
            "expected an OS-level errno, got: {err}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn read_cmdline_for_unknown_pid_returns_io_error() {
        // PID well above the typical Windows range — the OpenProcess
        // should fail with INVALID_PARAMETER or NOT_FOUND, which we
        // forward as the OS-level io::Error.
        let err = read_process_cmdline(0x7FFF_FFFE).expect_err("nonexistent pid");
        assert!(
            err.raw_os_error().is_some(),
            "expected an OS-level error code, got: {err}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn read_cmdline_round_trips_known_args_from_spawned_child() {
        use crate::observer::ObserverConfig;
        use crate::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
        use std::time::Duration;

        // Spawn a long-lived child with a distinctive argv, read its
        // cmdline back via NtQueryInformationProcess while it's still
        // alive, and assert the readback contains our argv markers.
        // `ping 127.0.0.1 -n 30` sleeps ~30s — plenty of time for the
        // readback before the child exits and is reaped.
        let cfg = ProcessConfig {
            command: CommandSpec::Argv(vec![
                "ping".into(),
                "127.0.0.1".into(),
                "-n".into(),
                "30".into(),
            ]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
        };
        let (process, _sub) = NativeProcess::with_observer(cfg, ObserverConfig::lifecycle());
        process.start().expect("spawn ping");
        let pid = process.pid().expect("pid");
        // Brief grace period so the process's PEB ProcessParameters is
        // fully initialized before we query.
        std::thread::sleep(Duration::from_millis(150));

        let cmdline = read_process_cmdline(pid).expect("read cmdline");
        process.kill().ok();
        process.close().ok();

        // Match relevant tokens — Windows command-line argv quoting
        // and capitalization can vary, so just check substrings.
        assert!(
            cmdline.to_lowercase().contains("ping"),
            "expected 'ping' in cmdline, got: {cmdline:?}"
        );
        assert!(
            cmdline.contains("127.0.0.1"),
            "expected target IP in cmdline, got: {cmdline:?}"
        );
        assert!(
            cmdline.contains("30"),
            "expected -n count in cmdline, got: {cmdline:?}"
        );
    }
}
