//! Windows fs implementation: permission primitives.

use std::path::Path;

/// Windows ignores Unix mode bits: NTFS uses ACLs and an archive
/// header's Unix mode carries no meaning there.
pub fn restore_mode(_path: &Path, _mode: Option<u32>) -> std::io::Result<()> {
    Ok(())
}

/// Adopt the source's permission set onto `file` with
/// FILE_ATTRIBUTE_READONLY cleared. The temp copy must carry the
/// original's flags (read-only travels) rather than its own defaults.
#[allow(clippy::permissions_set_readonly_false)] // Windows clears only FILE_ATTRIBUTE_READONLY.
pub fn make_writable_like(
    file: &std::fs::File,
    source: &std::fs::Permissions,
) -> std::io::Result<()> {
    let mut permissions = source.clone();
    permissions.set_readonly(false);
    file.set_permissions(permissions)
}

/// Executable bits carry no meaning on Windows (the `.exe`/`.cmd`
/// extension decides executability); nothing to apply.
pub fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Publish permissions for a materialized executable: Windows preserves
/// the source's permissions (read-only flags travel), since there are no
/// exec bits to apply.
pub fn make_executable_from(path: &Path, source: &std::fs::Permissions) -> std::io::Result<()> {
    std::fs::set_permissions(path, source.clone())
}

/// NTFS ACLs own directory privacy on Windows; owner-only mode bits
/// carry no meaning, so nothing to apply.
pub fn make_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Windows has no Unix mode bits; always `None` so callers can branch
/// at runtime instead of by cfg.
pub fn mode(_path: &Path) -> Option<u32> {
    None
}
