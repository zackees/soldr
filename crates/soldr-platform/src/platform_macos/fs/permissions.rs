//! macOS fs implementation: permission primitives.

use std::path::Path;

/// Restore an archived Unix mode onto `path`. `None` means the archive
/// carried no mode — nothing to apply.
pub fn restore_mode(path: &Path, mode: Option<u32>) -> std::io::Result<()> {
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Add the owner-write bit to an open file (keeping every other bit).
pub fn make_writable(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = file.metadata()?.permissions().mode();
    file.set_permissions(std::fs::Permissions::from_mode(mode | 0o200))
}

/// Add the execute bits to `path` (keeping every other bit): a freshly
/// written script lands at the umask default (0o644) and must become
/// runnable.
pub fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o111))
}
