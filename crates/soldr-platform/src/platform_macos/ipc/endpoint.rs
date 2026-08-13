//! macOS endpoint naming.

use std::path::{Path, PathBuf};

/// Return a short process-unique local-socket endpoint.
pub fn ephemeral(prefix: &str) -> String {
    format!(
        "/tmp/{prefix}-{}-{}.sock",
        std::process::id(),
        ephemeral_nonce() % 100_000
    )
}

fn ephemeral_nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos()
}

/// The macOS `sockaddr_un.sun_path` capacity in bytes.
pub fn sun_path_capacity() -> usize {
    104
}

/// A deterministic machine-local runtime directory for socket fallbacks:
/// `$TMPDIR/soldr-<uid>` (macOS convention), else `/tmp/soldr-<uid>`.
pub fn machine_runtime_dir() -> PathBuf {
    std::env::var_os("TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!("soldr-{}", unsafe { libc::getuid() }))
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
    let stats = unsafe { stats.assume_init() };
    let bytes: Vec<u8> = stats
        .f_fstypename
        .iter()
        .copied()
        .take_while(|byte| *byte != 0)
        .map(|byte| byte as u8)
        .collect();
    let fs = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    matches!(
        fs.as_str(),
        "nfs" | "smbfs" | "afpfs" | "webdav" | "osxfuse" | "macfuse"
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

/// Historical root-local daemon endpoint: only Windows derives a
/// user-scoped pipe name — Unix uses the filesystem path the caller
/// computes.
pub fn legacy_daemon_endpoint(_cache_root: &Path) -> Result<String, String> {
    Err("legacy daemon endpoint derivation is Windows-only".into())
}
