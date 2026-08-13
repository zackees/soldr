//! Windows atomic replacement and open-file retirement.

use std::fs::File;
use std::io;
use std::path::Path;

/// Atomically replace `target` with `source` (`MoveFileExW` with
/// replace-existing + write-through — `fs::rename` alone cannot replace
/// an existing file on Windows).
pub fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Upgrade an already-open file handle for retirement: open it with
/// delete access and share-delete so the image can be removed while the
/// process that mapped it still runs.
///
/// The upgrade re-opens the resolved handle rather than resolving an
/// ambient path again, preserving the caller's beneath-root guarantee.
pub fn open_for_retire(file: File) -> io::Result<File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        ReOpenFile, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };

    const DELETE_ACCESS: u32 = 0x0001_0000;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle() as _,
            GENERIC_READ | DELETE_ACCESS | FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ReOpenFile returned a new owned handle on success.
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

/// Retire `file` (opened via [`open_for_retire`]): mark it for
/// POSIX-semantics deletion through its own handle, so an image that is
/// still mapped by a running process disappears from the namespace
/// immediately and from disk when the last handle closes. `plain_remove`
/// is the caller's own capability-safe remove, which Windows deliberately
/// does not use here (a plain unlink of a mapped image fails).
///
/// There is deliberately no attribute-clearing fallback: a failure here
/// means the deletion contract is unavailable and the caller must not
/// pretend the removal succeeded.
pub fn retire_open_file(
    file: File,
    _plain_remove: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfoEx, SetFileInformationByHandle, FILE_DISPOSITION_FLAG_DELETE,
        FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX,
    };

    let mut disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: `file` was opened with DELETE access and remains alive for
    // the call. `disposition` has the exact layout required by
    // FileDispositionInfoEx.
    let success = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as _,
            FileDispositionInfoEx,
            std::ptr::from_mut(&mut disposition).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if success == 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!(
                "safe read-only hardlink detachment requires FileDispositionInfoEx; refusing an unsafe attribute-clearing fallback: {error}"
            ),
        ));
    }
    drop(file);
    Ok(())
}
