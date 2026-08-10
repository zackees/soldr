//! #539 — snapshot the file handles held by any LaunchedProcessTree
//! PID without admin privileges.
//!
//! Cross-platform dispatcher matching the [`super::cmdline`] shape:
//!
//! - **Linux**: walk `/proc/<pid>/fd/*` and `readlink()` each entry.
//!   Anonymous handles (`socket:[...]`, `pipe:[...]`, `anon_inode:...`)
//!   are returned as opaque labels alongside real filesystem paths.
//! - **macOS**: `proc_pidinfo(pid, PROC_PIDLISTFDS, ...)` enumerates
//!   the fd table, then `proc_pidinfo(pid, PROC_PIDFDVNODEPATHINFO,
//!   fd, ...)` resolves each vnode-backed fd to its filesystem path.
//!   Sockets / pipes / kqueues without a path are skipped.
//! - **Windows**: deferred to slice 4 of #539 (NtQuerySystemInformation
//!   handle snapshot + DuplicateHandle + NtQueryObject — substantially
//!   more involved than the Unix paths). Returns
//!   [`ErrorKind::Unsupported`] with the slice anchor in the message.

/// Snapshot the file handles currently held by `pid`, returned as
/// human-readable strings (filesystem paths where possible,
/// `socket:[...]` / `anon_inode:...` style labels otherwise).
///
/// The list is best-effort and racy by nature — handles open and
/// close between the enumeration call and the per-fd lookup. Any fd
/// that disappears mid-walk is silently skipped rather than failing
/// the whole snapshot.
pub fn read_process_file_handles(pid: u32) -> std::io::Result<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::read_process_file_handles(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos_impl::read_process_file_handles(pid)
    }
    #[cfg(target_os = "windows")]
    {
        windows_impl::read_process_file_handles(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = pid;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "#539: no LaunchedProcessTree handle-snapshot backend planned for this OS",
        ))
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    //! Windows handle snapshot via
    //! `NtQuerySystemInformation(SystemExtendedHandleInformation=64)`
    //! filtered by PID, then `DuplicateHandle` + `NtQueryObject` to
    //! resolve each File-typed handle's NT name.
    //!
    //! The size-doubling loop on `NtQuerySystemInformation` follows the
    //! standard pattern: call with a buffer, grow on
    //! `STATUS_INFO_LENGTH_MISMATCH`, retry. Filtering by
    //! `UniqueProcessId == target_pid` happens after the call but
    //! before the per-handle `DuplicateHandle` dance, so we never
    //! actually touch external processes' handles — we just see their
    //! presence in the system-wide table dump.
    //!
    //! `NtQueryObject(ObjectNameInformation)` can block indefinitely on
    //! certain non-File handle types (named pipes to remote endpoints,
    //! sockets to peer-disconnected sessions). We mitigate by first
    //! calling `NtQueryObject(ObjectTypeInformation)` and skipping any
    //! handle whose type name isn't `"File"`. ObjectTypeInformation is
    //! safe to query on any handle type — it doesn't traverse the
    //! object's name graph.

    use std::ffi::c_void;

    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcess};
    use winapi::um::winnt::{DUPLICATE_SAME_ACCESS, HANDLE, PROCESS_DUP_HANDLE};

    // ── Ntdll info classes ──

    /// `SystemExtendedHandleInformation` (info class 64). Returns
    /// `SYSTEM_HANDLE_INFORMATION_EX` (1 ULONG_PTR count + array of
    /// `SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX`).
    const SYSTEM_EXTENDED_HANDLE_INFORMATION: i32 = 64;

    /// `ObjectTypeInformation` (info class 2). Returns a
    /// `PUBLIC_OBJECT_TYPE_INFORMATION` (UNICODE_STRING TypeName +
    /// opaque tail). Safe to call on any handle type.
    const OBJECT_TYPE_INFORMATION: i32 = 2;

    /// `ObjectNameInformation` (info class 1). Returns a
    /// `PUBLIC_OBJECT_NAME_INFORMATION` (UNICODE_STRING Name).
    /// **Hazard:** can block forever on certain non-File handles; we
    /// guard by calling `ObjectTypeInformation` first.
    const OBJECT_NAME_INFORMATION: i32 = 1;

    const STATUS_SUCCESS: i32 = 0;
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004u32 as i32;

    /// Layout matches `SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX` from
    /// `<winternl.h>`. ULONG_PTR is pointer-sized (8 bytes on x86_64).
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct SystemHandleTableEntryInfoEx {
        object: usize,            // PVOID
        unique_process_id: usize, // ULONG_PTR
        handle_value: usize,      // ULONG_PTR  (raw HANDLE value as integer)
        granted_access: u32,
        creator_back_trace_index: u16,
        object_type_index: u16,
        handle_attributes: u32,
        reserved: u32,
    }

    /// Header for the buffer returned by
    /// `NtQuerySystemInformation(SystemExtendedHandleInformation)`: a
    /// single `ULONG_PTR` count followed by `count` entries.
    #[repr(C)]
    struct SystemHandleInformationExHeader {
        number_of_handles: usize, // ULONG_PTR
        reserved: usize,          // ULONG_PTR
    }

    /// Layout matches `UNICODE_STRING` from `<winternl.h>`.
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct UnicodeString {
        length: u16,         // bytes (excluding NUL)
        maximum_length: u16, // bytes
        buffer: *mut u16,
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtQuerySystemInformation(
            system_information_class: i32,
            system_information: *mut c_void,
            system_information_length: u32,
            return_length: *mut u32,
        ) -> i32;

        fn NtQueryObject(
            handle: HANDLE,
            object_information_class: i32,
            object_information: *mut c_void,
            object_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    pub(super) fn read_process_file_handles(pid: u32) -> std::io::Result<Vec<String>> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the system idle process — not queryable",
            ));
        }

        // 1. System-wide handle table dump (size-doubling loop).
        let raw = query_system_handles()?;
        let header_size = std::mem::size_of::<SystemHandleInformationExHeader>();
        if raw.len() < header_size {
            return Err(std::io::Error::other(
                "NtQuerySystemInformation returned < header bytes",
            ));
        }
        let header =
            unsafe { std::ptr::read(raw.as_ptr() as *const SystemHandleInformationExHeader) };
        let entry_size = std::mem::size_of::<SystemHandleTableEntryInfoEx>();
        let max_entries = (raw.len() - header_size) / entry_size;
        let entries_count = std::cmp::min(header.number_of_handles, max_entries);

        // 2. Open the target process for handle duplication. If this
        // fails (most often because the process exited between the
        // table dump and now, or we don't own the process), return
        // the OS error — that's the correct behavior for the
        // LaunchedProcessTree scope where we expect to own everything.
        let target_proc = unsafe { OpenProcess(PROCESS_DUP_HANDLE, FALSE, pid) };
        if target_proc.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let target_guard = ProcHandle(target_proc);

        // 3. Walk the entries, filter by pid, duplicate + type-check +
        // name-query each surviving handle.
        let mut handles = Vec::new();
        let entries_ptr =
            unsafe { raw.as_ptr().add(header_size) } as *const SystemHandleTableEntryInfoEx;
        for i in 0..entries_count {
            let entry = unsafe { std::ptr::read(entries_ptr.add(i)) };
            if entry.unique_process_id as u32 != pid {
                continue;
            }
            if let Some(path) = resolve_entry(target_guard.0, entry.handle_value as HANDLE) {
                handles.push(path);
            }
        }
        Ok(handles)
    }

    /// `NtQuerySystemInformation` size-doubling loop. Returns the raw
    /// bytes (header + entries) so the caller can parse them.
    fn query_system_handles() -> std::io::Result<Vec<u8>> {
        // Start with 256 KB — typical Windows hosts have 50k–100k
        // handles open across all processes; one ULONG_PTR + 28 bytes
        // each is ~2.8 MB at the 100k mark, so we double aggressively.
        let mut size: u32 = 256 * 1024;
        loop {
            let mut buf = vec![0u8; size as usize];
            let mut returned: u32 = 0;
            let status = unsafe {
                NtQuerySystemInformation(
                    SYSTEM_EXTENDED_HANDLE_INFORMATION,
                    buf.as_mut_ptr() as *mut c_void,
                    size,
                    &mut returned,
                )
            };
            if status == STATUS_SUCCESS {
                let used = returned.max(1) as usize;
                buf.truncate(used.min(buf.len()));
                return Ok(buf);
            }
            if status == STATUS_INFO_LENGTH_MISMATCH {
                // Double and retry. Cap at 256 MB to avoid runaway
                // growth on a malicious / pathological host.
                if size >= 256 * 1024 * 1024 {
                    return Err(std::io::Error::other(format!(
                        "NtQuerySystemInformation handle table exceeds 256 MiB (returned hint={returned})",
                    )));
                }
                size = size
                    .saturating_mul(2)
                    .max(returned.saturating_add(64 * 1024));
                continue;
            }
            return Err(std::io::Error::other(format!(
                "NtQuerySystemInformation returned status=0x{:08x}",
                status as u32,
            )));
        }
    }

    /// Duplicate one foreign-process handle into the calling process,
    /// check the object type, resolve the name if it's `"File"`, then
    /// close the duplicated handle. Errors / non-File handles return
    /// `None` rather than aborting the whole snapshot.
    fn resolve_entry(target_proc: HANDLE, foreign_handle: HANDLE) -> Option<String> {
        use winapi::um::handleapi::DuplicateHandle;
        let mut local_handle: HANDLE = std::ptr::null_mut();
        let ok = unsafe {
            DuplicateHandle(
                target_proc,
                foreign_handle,
                GetCurrentProcess(),
                &mut local_handle,
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == FALSE || local_handle.is_null() {
            return None;
        }
        let local_guard = ProcHandle(local_handle);

        // Type-check first: ObjectTypeInformation is safe on any
        // handle. ObjectNameInformation is NOT safe on
        // pipes/sockets, so we filter before calling it.
        let type_name = query_object_string(local_guard.0, OBJECT_TYPE_INFORMATION)?;
        if type_name != "File" {
            return None;
        }
        query_object_string(local_guard.0, OBJECT_NAME_INFORMATION).filter(|s| !s.is_empty())
    }

    /// Call `NtQueryObject(class, ...)` with a size-doubling loop,
    /// extract the leading `UNICODE_STRING`, and decode it as UTF-8
    /// (lossy on the rare invalid-surrogate edge). The buffer is
    /// read from the local allocation, not the kernel-returned
    /// `buffer` pointer, so we don't deref a kernel-side address.
    fn query_object_string(handle: HANDLE, info_class: i32) -> Option<String> {
        let mut size: u32 = 4 * 1024;
        loop {
            let mut buf = vec![0u8; size as usize];
            let mut returned: u32 = 0;
            let status = unsafe {
                NtQueryObject(
                    handle,
                    info_class,
                    buf.as_mut_ptr() as *mut c_void,
                    size,
                    &mut returned,
                )
            };
            if status == STATUS_SUCCESS {
                buf.truncate((returned as usize).min(buf.len()));
                return parse_leading_unicode_string(&buf);
            }
            if status == STATUS_INFO_LENGTH_MISMATCH {
                if size >= 1024 * 1024 {
                    return None;
                }
                size = size.saturating_mul(2).max(returned);
                continue;
            }
            return None;
        }
    }

    /// Read the leading `UNICODE_STRING` from `buf` and return the
    /// wide-char data as a `String`.
    ///
    /// We must trust `us.buffer` (the kernel-supplied pointer) rather
    /// than assuming the string lives immediately after the header.
    /// For `ProcessCommandLineInformation` the string is appended
    /// directly, but for `PUBLIC_OBJECT_TYPE_INFORMATION` it lives
    /// past 88 bytes of trailing `Reserved[22]` fields. The kernel
    /// writes `us.buffer` as a pointer into our supplied allocation
    /// regardless of where it chose to place the bytes.
    fn parse_leading_unicode_string(buf: &[u8]) -> Option<String> {
        let header_size = std::mem::size_of::<UnicodeString>();
        if buf.len() < header_size {
            return None;
        }
        let us = unsafe { std::ptr::read(buf.as_ptr() as *const UnicodeString) };
        let len_bytes = us.length as usize;
        if len_bytes == 0 || us.buffer.is_null() {
            return Some(String::new());
        }
        // Sanity check: the kernel-supplied pointer should point
        // inside our buf allocation. Reject otherwise to avoid an
        // accidental wild deref.
        let buf_start = buf.as_ptr() as usize;
        let buf_end = buf_start + buf.len();
        let buffer_addr = us.buffer as usize;
        if buffer_addr < buf_start || buffer_addr.saturating_add(len_bytes) > buf_end {
            return None;
        }
        let wide: &[u16] =
            unsafe { std::slice::from_raw_parts(us.buffer as *const u16, len_bytes / 2) };
        Some(String::from_utf16_lossy(wide))
    }

    /// RAII wrapper that closes a HANDLE on drop. Used for both the
    /// OpenProcess result and the per-entry DuplicateHandle result.
    struct ProcHandle(HANDLE);
    impl Drop for ProcHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    pub(super) fn read_process_file_handles(pid: u32) -> std::io::Result<Vec<String>> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the kernel scheduler — not queryable",
            ));
        }
        let dir = format!("/proc/{pid}/fd");
        let entries = std::fs::read_dir(&dir)?;
        let mut handles = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            // Each entry is a symlink to either a filesystem path
            // (e.g. /etc/hosts) or an anonymous kernel object
            // (`socket:[12345]`, `pipe:[67890]`, `anon_inode:...`).
            // `read_link` returns the target as a PathBuf — keep the
            // raw lossy-decoded string so anonymous targets survive
            // intact for downstream pattern-matching.
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            handles.push(target.to_string_lossy().into_owned());
        }
        Ok(handles)
    }
}

#[cfg(target_os = "macos")]
mod macos_impl {
    // libc 0.2 exposes `proc_pidinfo` / `proc_pidfdinfo` on macOS but
    // does NOT export `vnode_fdinfowithpath` / `proc_fdinfo` /
    // PROC_PIDLISTFDS / PROC_PIDFDVNODEPATHINFO. Declare them inline
    // from `<sys/proc_info.h>` — layouts and values have been
    // ABI-stable since OS X 10.5.
    const PROC_PIDLISTFDS: libc::c_int = 1;
    const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;
    const PROX_FDTYPE_VNODE: u32 = 1;
    /// `MAXPATHLEN` from `<sys/syslimits.h>`.
    const MAXPATHLEN: usize = 1024;

    /// `struct proc_fdinfo { int32_t proc_fd; uint32_t proc_fdtype; }`.
    /// 8 bytes; size of array entry returned by `proc_pidinfo(PROC_PIDLISTFDS)`.
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct ProcFdInfo {
        proc_fd: i32,
        proc_fdtype: u32,
    }

    /// Opaque buffer matching `vnode_fdinfowithpath` (24 byte
    /// `proc_fileinfo` header + 152 byte `vnode_info` + 1024 byte
    /// `vip_path` = 1200 bytes total). We only read `vip_path`,
    /// which lives at offset 24 + 152 = 176.
    const VNODE_FDINFOWITHPATH_SIZE: usize = 1200;
    const VIP_PATH_OFFSET: usize = 176;

    #[repr(C)]
    struct VnodeFdInfoWithPath {
        _opaque: [u8; VNODE_FDINFOWITHPATH_SIZE],
    }

    pub(super) fn read_process_file_handles(pid: u32) -> std::io::Result<Vec<String>> {
        if pid == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pid 0 is the kernel scheduler — not queryable",
            ));
        }
        // Size probe: PROC_PIDLISTFDS with null buffer returns required
        // bytes for the proc_fdinfo array.
        let size = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                PROC_PIDLISTFDS,
                0,
                std::ptr::null_mut(),
                0,
            )
        };
        if size <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let entry_size = std::mem::size_of::<ProcFdInfo>();
        let count = (size as usize) / entry_size;
        let mut fds: Vec<ProcFdInfo> = vec![
            ProcFdInfo {
                proc_fd: 0,
                proc_fdtype: 0
            };
            count
        ];
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr() as *mut libc::c_void,
                (count * entry_size) as libc::c_int,
            )
        };
        if written <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let written_count = (written as usize) / entry_size;
        fds.truncate(written_count);

        let mut handles = Vec::new();
        for fd in &fds {
            // We only resolve vnode-backed fds (regular files,
            // directories, devices). Sockets/pipes/kqueues have no
            // POSIX path; skip them.
            if fd.proc_fdtype != PROX_FDTYPE_VNODE {
                continue;
            }
            let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
            let n = unsafe {
                libc::proc_pidfdinfo(
                    pid as libc::c_int,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    &mut info as *mut VnodeFdInfoWithPath as *mut libc::c_void,
                    std::mem::size_of::<VnodeFdInfoWithPath>() as libc::c_int,
                )
            };
            if n <= 0 {
                // fd closed between listfds and fdinfo — skip the race.
                continue;
            }
            let path_bytes = &info._opaque[VIP_PATH_OFFSET..VIP_PATH_OFFSET + MAXPATHLEN];
            let nul = path_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(path_bytes.len());
            if nul == 0 {
                continue;
            }
            let path = String::from_utf8_lossy(&path_bytes[..nul]).into_owned();
            handles.push(path);
        }
        Ok(handles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn read_handles_for_pid_zero_returns_invalid_input() {
        let err = read_process_file_handles(0).expect_err("pid 0 should be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn self_snapshot_includes_a_temp_file_we_just_opened() {
        // Open a temp file, snapshot our own fds, assert the temp
        // file's path is in the result.
        //
        // Unix backends return POSIX paths that match `path` /
        // `canonical` directly. The Windows backend returns NT
        // object names like `\Device\HarddiskVolume3\Users\...\tmpXXXX`
        // which won't match a DOS-style path equal-for-equal — so we
        // also match on filename suffix, which is reliable across
        // the NT/DOS path translation gap.
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_default();

        let handles = read_process_file_handles(std::process::id()).expect("read self handles");
        let canonical_str = canonical.to_string_lossy();
        let raw_str = path.to_string_lossy();
        let found = handles.iter().any(|h| {
            h == canonical_str.as_ref()
                || h == raw_str.as_ref()
                || (!filename.is_empty() && h.ends_with(&filename))
        });
        assert!(
            found,
            "expected temp file (filename={filename}, canonical={canonical_str}, raw={raw_str}) in handles, got {handles:?}",
        );
        // Drop tmp explicitly so it stays alive until after the snapshot.
        drop(tmp);
    }
}
