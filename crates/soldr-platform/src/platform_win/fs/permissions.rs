//! Windows fs implementation: permission primitives.

use std::path::Path;

/// Windows ignores Unix mode bits: NTFS uses ACLs and an archive
/// header's Unix mode carries no meaning there.
pub fn restore_mode(_path: &Path, _mode: Option<u32>) -> std::io::Result<()> {
    Ok(())
}
