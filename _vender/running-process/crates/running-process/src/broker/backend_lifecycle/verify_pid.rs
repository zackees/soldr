//! Process identity verification for backend handles.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::broker::backend_lifecycle::identity::{self, DaemonProcess};
use crate::broker::host_identity;

/// Verify a daemon process identity and return an OS liveness handle.
pub fn verify_daemon_process(expected: &DaemonProcess) -> Result<ProcessHandle, VerifyPidError> {
    if expected.pid == 0 {
        return Err(VerifyPidError::InvalidPid(expected.pid));
    }

    let current_boot_id = host_identity::current().boot_id;
    if !expected.boot_id.is_empty()
        && !current_boot_id.is_empty()
        && expected.boot_id != current_boot_id
    {
        return Err(VerifyPidError::BootIdMismatch {
            expected: expected.boot_id.clone(),
            actual: current_boot_id,
        });
    }

    let handle = ProcessHandle::open(expected.pid)?;
    let exe_path = process_exe_path(expected.pid).map_err(|source| VerifyPidError::ExePath {
        pid: expected.pid,
        source,
    })?;
    if !same_exe_path(&exe_path, &expected.exe_path) {
        return Err(VerifyPidError::ExePathMismatch {
            pid: expected.pid,
            expected: expected.exe_path.clone(),
            actual: exe_path,
        });
    }

    let actual_sha256 =
        identity::sha256_file(&exe_path).map_err(|source| VerifyPidError::ExeHash {
            pid: expected.pid,
            path: exe_path.clone(),
            source,
        })?;
    if actual_sha256 != expected.exe_sha256 {
        return Err(VerifyPidError::ExeSha256Mismatch { pid: expected.pid });
    }

    Ok(handle)
}

/// Return whether a process ID currently resolves to a live process.
pub fn process_is_alive(pid: u32) -> bool {
    ProcessHandle::open(pid)
        .map(|handle| handle.is_alive())
        .unwrap_or(false)
}

/// Send a graceful terminate signal where the platform has one.
pub fn signal_terminate(pid: u32) -> Result<(), VerifyPidError> {
    platform_signal_terminate(pid)
}

/// Force-kill a process ID.
pub fn force_kill_pid(pid: u32) -> Result<(), VerifyPidError> {
    platform_force_kill(pid)
}

/// Errors returned while verifying a daemon process.
#[derive(Debug, thiserror::Error)]
pub enum VerifyPidError {
    /// PID zero or a value outside the native PID range is never valid.
    #[error("invalid daemon pid: {0}")]
    InvalidPid(u32),
    /// The process is not currently alive.
    #[error("process not found: {pid}")]
    NotFound {
        /// Process ID that could not be opened.
        pid: u32,
    },
    /// The manifest was written during a prior host boot.
    #[error("daemon boot id mismatch: expected {expected}, current {actual}")]
    BootIdMismatch {
        /// Boot ID stored with the daemon identity.
        expected: String,
        /// Current host boot ID.
        actual: String,
    },
    /// The executable could not be hashed.
    #[error("failed to hash executable for pid {pid} at {path:?}: {source}")]
    ExeHash {
        /// Process ID being verified.
        pid: u32,
        /// Executable path selected for hashing.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },
    /// The executable path for the process could not be read.
    #[error("failed to resolve executable path for pid {pid}: {source}")]
    ExePath {
        /// Process ID being verified.
        pid: u32,
        /// Underlying platform error.
        source: io::Error,
    },
    /// The executable path did not match the manifest identity.
    #[error(
        "daemon executable path mismatch for pid {pid}: expected {expected:?}, actual {actual:?}"
    )]
    ExePathMismatch {
        /// Process ID being verified.
        pid: u32,
        /// Executable path stored with the daemon identity.
        expected: PathBuf,
        /// Executable path reported by the operating system.
        actual: PathBuf,
    },
    /// The executable hash did not match the manifest identity.
    #[error("daemon executable sha256 mismatch for pid {pid}")]
    ExeSha256Mismatch {
        /// Process ID being verified.
        pid: u32,
    },
    /// A platform process-handle operation failed.
    #[error("process handle operation failed for pid {pid}: {source}")]
    Handle {
        /// Process ID being opened or signalled.
        pid: u32,
        /// Underlying platform error.
        source: io::Error,
    },
    /// The platform has no graceful shutdown primitive in this foundation.
    #[error("graceful terminate is unsupported on this platform")]
    GracefulTerminateUnsupported,
}

#[cfg(unix)]
mod platform {
    use std::io;

    #[cfg(target_os = "macos")]
    use std::ptr;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    #[cfg(target_os = "macos")]
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::VerifyPidError;

    /// Platform liveness handle for a backend process.
    pub struct ProcessHandle {
        pid: u32,
        #[cfg(target_os = "linux")]
        pid_fd: Option<OwnedFd>,
        #[cfg(target_os = "macos")]
        exit_kqueue: OwnedFd,
        #[cfg(target_os = "macos")]
        exited: AtomicBool,
    }

    impl ProcessHandle {
        pub(crate) fn open(pid: u32) -> Result<Self, VerifyPidError> {
            validate_pid(pid)?;
            #[cfg(target_os = "macos")]
            {
                Ok(Self {
                    pid,
                    exit_kqueue: open_exit_kqueue(pid)?,
                    exited: AtomicBool::new(false),
                })
            }

            #[cfg(target_os = "linux")]
            {
                if !process_exists(pid) {
                    return Err(VerifyPidError::NotFound { pid });
                }
                Ok(Self {
                    pid,
                    pid_fd: try_pidfd_open(pid)?,
                })
            }

            #[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
            {
                if !process_exists(pid) {
                    return Err(VerifyPidError::NotFound { pid });
                }
                Ok(Self { pid })
            }
        }

        /// Process ID associated with this handle.
        pub fn pid(&self) -> u32 {
            self.pid
        }

        /// Return whether the process represented by this handle is alive.
        pub fn is_alive(&self) -> bool {
            #[cfg(target_os = "linux")]
            {
                if let Some(pid_fd) = self.pid_fd.as_ref() {
                    return pidfd_is_alive(pid_fd);
                }
                process_exists(self.pid)
            }

            #[cfg(target_os = "macos")]
            {
                !self.exited.load(Ordering::Relaxed)
                    && kqueue_process_is_alive(&self.exit_kqueue, &self.exited)
            }

            #[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
            {
                process_exists(self.pid)
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn process_exists(pid: u32) -> bool {
        let Ok(native_pid) = validate_pid(pid) else {
            return false;
        };
        let rc = unsafe { libc::kill(native_pid, 0) };
        if rc == 0 {
            return true;
        }
        matches!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM))
    }

    pub(crate) fn platform_signal_terminate(pid: u32) -> Result<(), VerifyPidError> {
        let native_pid = validate_pid(pid)?;
        let rc = unsafe { libc::kill(native_pid, libc::SIGTERM) };
        if rc == 0 {
            Ok(())
        } else {
            Err(VerifyPidError::Handle {
                pid,
                source: io::Error::last_os_error(),
            })
        }
    }

    pub(crate) fn platform_force_kill(pid: u32) -> Result<(), VerifyPidError> {
        let native_pid = validate_pid(pid)?;
        let rc = unsafe { libc::kill(native_pid, libc::SIGKILL) };
        if rc == 0 {
            Ok(())
        } else {
            Err(VerifyPidError::Handle {
                pid,
                source: io::Error::last_os_error(),
            })
        }
    }

    fn validate_pid(pid: u32) -> Result<libc::pid_t, VerifyPidError> {
        if pid == 0 || pid > libc::pid_t::MAX as u32 {
            Err(VerifyPidError::InvalidPid(pid))
        } else {
            Ok(pid as libc::pid_t)
        }
    }

    #[cfg(target_os = "macos")]
    fn open_exit_kqueue(pid: u32) -> Result<OwnedFd, VerifyPidError> {
        let native_pid = validate_pid(pid)?;
        let raw_fd = unsafe { libc::kqueue() };
        if raw_fd < 0 {
            return Err(VerifyPidError::Handle {
                pid,
                source: io::Error::last_os_error(),
            });
        }

        let kqueue_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let change = libc::kevent {
            ident: native_pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: ptr::null_mut(),
        };
        let rc = unsafe {
            libc::kevent(
                kqueue_fd.as_raw_fd(),
                &change,
                1,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        };
        if rc == 0 {
            return Ok(kqueue_fd);
        }

        let source = io::Error::last_os_error();
        if matches!(source.raw_os_error(), Some(libc::ESRCH)) {
            Err(VerifyPidError::NotFound { pid })
        } else {
            Err(VerifyPidError::Handle { pid, source })
        }
    }

    #[cfg(target_os = "macos")]
    fn kqueue_process_is_alive(kqueue_fd: &OwnedFd, exited: &AtomicBool) -> bool {
        let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let rc = unsafe {
            libc::kevent(
                kqueue_fd.as_raw_fd(),
                ptr::null(),
                0,
                event.as_mut_ptr(),
                1,
                &timeout,
            )
        };
        if rc == 0 {
            return true;
        }

        exited.store(true, Ordering::Relaxed);
        false
    }

    #[cfg(target_os = "linux")]
    fn try_pidfd_open(pid: u32) -> Result<Option<OwnedFd>, VerifyPidError> {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0_u32) };
        if raw >= 0 {
            let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
            return Ok(Some(fd));
        }

        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENOSYS | libc::EINVAL | libc::EPERM) => Ok(None),
            Some(libc::ESRCH) => Err(VerifyPidError::NotFound { pid }),
            _ => Ok(None),
        }
    }

    #[cfg(target_os = "linux")]
    fn pidfd_is_alive(pid_fd: &OwnedFd) -> bool {
        let mut poll_fd = libc::pollfd {
            fd: pid_fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut poll_fd, 1, 0) };
        rc == 0
    }
}

#[cfg(windows)]
mod platform {
    use std::io;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE,
    };

    use super::VerifyPidError;

    const STILL_ACTIVE: u32 = 259;

    /// Platform liveness handle for a backend process.
    pub struct ProcessHandle {
        pid: u32,
        handle: HANDLE,
    }

    impl ProcessHandle {
        pub(crate) fn open(pid: u32) -> Result<Self, VerifyPidError> {
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if handle.is_null() {
                return Err(VerifyPidError::NotFound { pid });
            }
            Ok(Self { pid, handle })
        }

        /// Process ID associated with this handle.
        pub fn pid(&self) -> u32 {
            self.pid
        }

        /// Return whether the process represented by this handle is alive.
        pub fn is_alive(&self) -> bool {
            let mut exit_code = 0_u32;
            let ok = unsafe { GetExitCodeProcess(self.handle, &mut exit_code) };
            ok != 0 && exit_code == STILL_ACTIVE
        }
    }

    impl Drop for ProcessHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    pub(crate) fn platform_signal_terminate(_pid: u32) -> Result<(), VerifyPidError> {
        Err(VerifyPidError::GracefulTerminateUnsupported)
    }

    pub(crate) fn platform_force_kill(pid: u32) -> Result<(), VerifyPidError> {
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() {
            return Err(VerifyPidError::NotFound { pid });
        }
        let ok = unsafe { TerminateProcess(handle, 1) };
        let source = io::Error::last_os_error();
        unsafe {
            CloseHandle(handle);
        }
        if ok == 0 {
            Err(VerifyPidError::Handle { pid, source })
        } else {
            Ok(())
        }
    }
}

pub use platform::ProcessHandle;
use platform::{platform_force_kill, platform_signal_terminate};

fn process_exe_path(pid: u32) -> Result<PathBuf, io::Error> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe"))
    }

    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        let mut path = vec![0_u16; 32768];
        let mut len = path.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len) };
        let source = io::Error::last_os_error();
        unsafe {
            CloseHandle(handle);
        }
        if ok == 0 {
            return Err(source);
        }

        path.truncate(len as usize);
        Ok(PathBuf::from(String::from_utf16_lossy(&path)))
    }

    #[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
    {
        let mut system = sysinfo::System::new_all();
        system.refresh_processes();
        if let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) {
            if let Some(exe) = process.exe() {
                return Ok(exe.to_path_buf());
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "process executable path not found",
        ))
    }
}

fn same_exe_path(actual: &Path, expected: &Path) -> bool {
    let actual = fs::canonicalize(actual).unwrap_or_else(|_| actual.to_path_buf());
    let expected = fs::canonicalize(expected).unwrap_or_else(|_| expected.to_path_buf());

    #[cfg(windows)]
    {
        comparable_windows_path(&actual) == comparable_windows_path(&expected)
    }

    #[cfg(not(windows))]
    {
        actual == expected
    }
}

#[cfg(windows)]
fn comparable_windows_path(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    path.to_ascii_lowercase()
}
