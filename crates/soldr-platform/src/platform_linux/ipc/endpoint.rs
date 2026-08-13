//! Linux endpoint naming.

use std::path::{Path, PathBuf};

/// The Linux `sockaddr_un.sun_path` capacity in bytes.
pub fn sun_path_capacity() -> usize {
    108
}

/// A deterministic machine-local runtime directory for socket fallbacks:
/// `$XDG_RUNTIME_DIR` when set, else `/tmp/soldr-<uid>`.
pub fn machine_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!("/tmp/soldr-{}", unsafe { libc::getuid() }))
        })
}

/// True when the filesystem hosting `path` is known to reject or fail to
/// coordinate Unix-domain socket binds (networked / userspace
/// filesystems).
pub fn path_is_on_non_bindable_filesystem(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    let existing = path
        .ancestors()
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| Path::new("/"));

    let Ok(c_path) = std::ffi::CString::new(existing.as_os_str().as_bytes()) else {
        return true;
    };
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(c_path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return false;
    }
    let magic = unsafe { stats.assume_init() }.f_type;
    // Network and userspace filesystems that commonly reject or fail to
    // coordinate Unix-domain socket binds. Values come from Linux magic.h.
    matches!(
        magic,
        0x0000_6969 // NFS
            | 0xFF53_4D42 // CIFS
            | 0xFE53_4D42 // SMB2
            | 0x7375_7245 // CODA
            | 0x5346_414F // AFS
            | 0x0000_564C // NCP
            | 0x00C3_6400 // CEPH
            | 0x0102_1997 // 9P
            | 0x6573_5546 // FUSE (sshfs and peers)
    )
}

/// The raw `OsStr` bytes that feed the socket name.
pub fn socket_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes().to_vec()
}

/// Whether a socket path (including its trailing NUL) fits the
/// `capacity` budget.
pub fn socket_path_fits(path: &Path, capacity: usize) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes().len().saturating_add(1) <= capacity
}
