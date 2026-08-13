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

/// Adopt the source's full mode onto `file` with the owner-write bit
/// added. A freshly created private copy lands at the umask default
/// (0o644) and must carry the original's permission set — including the
/// execute bits — instead of its own.
pub fn make_writable_like(
    file: &std::fs::File,
    source: &std::fs::Permissions,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(source.mode() | 0o200))
}

/// Add the execute bits to `path` (keeping every other bit): a freshly
/// written script lands at the umask default (0o644) and must become
/// runnable.
pub fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o111))
}

/// Publish permissions for a materialized executable: the published
/// shim must be runnable regardless of the source's own mode, so apply
/// a fixed 0o755.
pub fn make_executable_from(path: &Path, _source: &std::fs::Permissions) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Restrict a directory to its owner (0o700).
pub fn make_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Read the file's Unix permission bits (e.g. `0o755`). `None` when the
/// metadata read fails; on hosts without mode semantics (Windows) the
/// concrete tree always returns `None`, letting callers branch at
/// runtime instead of by cfg.
pub fn mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).ok().map(|m| m.permissions().mode())
}
