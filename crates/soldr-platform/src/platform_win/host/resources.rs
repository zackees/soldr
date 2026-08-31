//! Windows host resources: CPU topology and process/memory pressure.

use std::sync::OnceLock;

/// cgroup v2 is a Linux facility.
pub fn cgroup_v2_dir() -> Option<std::path::PathBuf> {
    None
}

/// Physical CPU cores on this machine, or `None` when the topology could
/// not be read. Memoized: the daemon asks once at startup.
pub fn physical_cores() -> Option<usize> {
    static CACHED: OnceLock<Option<usize>> = OnceLock::new();
    *CACHED.get_or_init(|| detect_cores().filter(|cores| *cores > 0))
}

/// `GetLogicalProcessorInformationEx(RelationProcessorCore, ..)` returns
/// exactly one variable-length record per physical core, so counting
/// records is the answer.
fn detect_cores() -> Option<usize> {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
    };

    // Records are 8-byte aligned; backing the buffer with `u64` gives
    // that alignment for free, and the header fields are still read
    // unaligned so a driver reporting an odd `Size` cannot cause UB.
    const HEADER_BYTES: usize = 8; // Relationship: u32, Size: u32

    let mut len: u32 = 0;
    // The sizing call is *expected* to fail with
    // ERROR_INSUFFICIENT_BUFFER; only `len` matters here.
    unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len)
    };
    if (len as usize) < HEADER_BYTES {
        return None;
    }
    let mut buffer = vec![0u64; (len as usize).div_ceil(8)];
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            buffer.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if ok == 0 {
        return None;
    }

    let base = buffer.as_ptr().cast::<u8>();
    let len = len as usize;
    let mut offset = 0usize;
    let mut cores = 0usize;
    while offset + HEADER_BYTES <= len {
        // SAFETY: `offset + HEADER_BYTES <= len` and `buffer` holds at
        // least `len` bytes, so both reads are in bounds. Unaligned
        // reads have no alignment requirement.
        let (relationship, size) = unsafe {
            let record = base.add(offset);
            (
                record.cast::<u32>().read_unaligned(),
                record.add(4).cast::<u32>().read_unaligned() as usize,
            )
        };
        // A zero or out-of-range `Size` would loop forever or walk off
        // the buffer. Stop and report what was counted so far.
        if size < HEADER_BYTES || offset + size > len {
            break;
        }
        if relationship == RelationProcessorCore as u32 {
            cores += 1;
        }
        offset += size;
    }
    (cores > 0).then_some(cores)
}

/// Walk the live process table once via ToolHelp, returning `(pid, image
/// name)` rows. `None` if the snapshot could not be taken.
pub fn process_table() -> Option<Vec<(u32, String)>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: FFI call with documented args; returns a handle we validate
    // against INVALID_HANDLE_VALUE before use and CloseHandle on every path.
    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return None;
    }

    let mut rows = Vec::new();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    // SAFETY: `entry` is a fully-owned, correctly-sized PROCESSENTRY32W; the
    // API fills `szExeFile` in-place. `handle` is valid (checked above).
    let mut ok = unsafe { Process32FirstW(handle, &mut entry) };
    while ok != 0 {
        rows.push((entry.th32ProcessID, image_name_from_entry(&entry.szExeFile)));
        // SAFETY: same invariants as Process32FirstW; iterates the snapshot.
        ok = unsafe { Process32NextW(handle, &mut entry) };
    }

    // SAFETY: `handle` came from CreateToolhelp32Snapshot and is closed exactly
    // once here, including when iteration stopped early.
    unsafe {
        CloseHandle(handle);
    }

    Some(rows)
}

/// Decode a NUL-terminated UTF-16 `szExeFile` field into a Rust `String`.
fn image_name_from_entry(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Read the system commit charge via `GlobalMemoryStatusEx`, returning
/// `(used_mb, limit_mb)`. `None` if the call failed.
pub fn commit_charge_mb() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;

    // SAFETY: `status` is a correctly-sized, dwLength-initialized MEMORYSTATUSEX
    // that the API fills in-place; no handles or allocations are involved.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 {
        return None;
    }

    const MB: u64 = 1024 * 1024;
    // ullTotalPageFile is the commit limit; ullAvailPageFile what remains.
    let limit = status.ullTotalPageFile / MB;
    let used = status
        .ullTotalPageFile
        .saturating_sub(status.ullAvailPageFile)
        / MB;
    Some((used, limit))
}
