//! macOS file identity: length, mtime, and the dev/ino/ctime tuple.

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

/// Stable identity for `path`, or `None` when the file cannot be read.
pub fn file_identity(path: &Path) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path).ok()?;
    let modified_ns = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(FileIdentity {
        len: metadata.len(),
        modified_ns,
        dev: Some(metadata.dev()),
        ino: Some(metadata.ino()),
        ctime: Some(metadata.ctime()),
        ctime_nsec: Some(metadata.ctime_nsec()),
        volume_serial_number: None,
        file_index: None,
        creation_time: None,
    })
}

/// True when both paths name the same file (same device and inode).
/// Paths that cannot be resolved are never the same file.
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (file_identity(a), file_identity(b)) {
        (Some(a), Some(b)) => a.dev == b.dev && a.ino == b.ino && a.dev.is_some(),
        _ => false,
    }
}
