//! Linux fs implementation: permission primitives.

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
