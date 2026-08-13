//! Windows file identity: length, mtime, and the volume-serial/file-index
//! pair from `GetFileInformationByHandle`.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Stable file identity.
///
/// The platform-specific members are `Option`s so the type stays neutral
/// and serializable: Unix fills `dev`/`ino`/`ctime`, Windows fills
/// `volume_serial_number`/`file_index`/`creation_time`. Two identities
/// compare equal only when every populated member matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    /// File length in bytes.
    pub len: u64,
    /// Modified time as nanoseconds since the Unix epoch.
    pub modified_ns: u128,
    /// Unix device id.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dev: Option<u64>,
    /// Unix inode.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ino: Option<u64>,
    /// Unix change-time seconds.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ctime: Option<i64>,
    /// Unix change-time nanoseconds.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ctime_nsec: Option<i64>,
    /// Windows volume serial number.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub volume_serial_number: Option<u64>,
    /// Windows file index (the inode-equivalent identity component).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub file_index: Option<u64>,
    /// Windows creation time in FILETIME units.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub creation_time: Option<u64>,
}

/// Stable identity for `path`, or `None` when the file or its identity
/// cannot be read. Returns `None` rather than a weak size+mtime fallback
/// when the handle-based identity is unavailable, so callers that memoize
/// identity fall back to their content hash instead.
pub fn file_identity(path: &Path) -> Option<FileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let metadata = std::fs::metadata(path).ok()?;
    let modified_ns = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let file = std::fs::File::open(path).ok()?;
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `file` owns a valid handle and `info` has the exact storage
    // required by GetFileInformationByHandle.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) } == 0 {
        return None;
    }
    // SAFETY: the successful call initialized the full structure.
    let info = unsafe { info.assume_init() };
    Some(FileIdentity {
        len: metadata.len(),
        modified_ns,
        dev: None,
        ino: None,
        ctime: None,
        ctime_nsec: None,
        volume_serial_number: Some(u64::from(info.dwVolumeSerialNumber)),
        file_index: Some(
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ),
        creation_time: Some(
            (u64::from(info.ftCreationTime.dwHighDateTime) << 32)
                | u64::from(info.ftCreationTime.dwLowDateTime),
        ),
    })
}

/// True when both paths name the same file (same volume serial + file
/// index). Paths that cannot be resolved are never the same file.
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (file_identity(a), file_identity(b)) {
        (Some(a), Some(b)) => {
            a.volume_serial_number == b.volume_serial_number
                && a.file_index == b.file_index
                && a.volume_serial_number.is_some()
        }
        _ => false,
    }
}
