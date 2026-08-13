//! Windows fs implementation: permission primitives.

use std::path::Path;

/// Windows ignores Unix mode bits: NTFS uses ACLs and an archive
/// header's Unix mode carries no meaning there.
pub fn restore_mode(_path: &Path, _mode: Option<u32>) -> std::io::Result<()> {
    Ok(())
}

/// Make an open file writable by clearing FILE_ATTRIBUTE_READONLY.
#[allow(clippy::permissions_set_readonly_false)] // Windows clears only FILE_ATTRIBUTE_READONLY.
pub fn make_writable(file: &std::fs::File) -> std::io::Result<()> {
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(false);
    file.set_permissions(permissions)
}

/// Executable bits carry no meaning on Windows (the `.exe`/`.cmd`
/// extension decides executability); nothing to apply.
pub fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
