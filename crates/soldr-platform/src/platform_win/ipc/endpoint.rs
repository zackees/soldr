//! Windows endpoint naming.

use std::path::{Path, PathBuf};

use crate::platform::ipc::endpoint::windows_pipe_from_executable_with_suffix;

/// The `sun_path` capacity concept does not apply to named pipes; the
/// caller only consults this on the Unix branch.
pub fn sun_path_capacity() -> usize {
    0
}

/// The caller only consults this on the Unix branch.
pub fn machine_runtime_dir() -> PathBuf {
    PathBuf::new()
}

/// Named-pipe paths carry no filesystem-magic constraints; the caller
/// only consults this on the Unix branch.
pub fn path_is_on_non_bindable_filesystem(_path: &Path) -> bool {
    false
}

/// The raw bytes of a socket path (Unix: the exact `OsStr` bytes
/// that feed the socket name; Windows: the lossy UTF-8 form — the
/// caller only consults this on the Unix branch).
pub fn socket_path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().to_string_lossy().as_bytes().to_vec()
}

/// Whether a socket path of `capacity`-shaped budgets fits; named pipes
/// use their own 256-character ceiling, so this answers the Windows
/// length question.
pub fn socket_path_fits(path: &Path, capacity: usize) -> bool {
    path.as_os_str().len() < capacity
}
