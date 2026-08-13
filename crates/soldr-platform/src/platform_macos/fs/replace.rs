//! macOS atomic replacement and open-file retirement.

use std::fs::File;
use std::io;
use std::path::Path;

/// Atomically replace `target` with `source` (`rename` is atomic and
/// replaces existing files on Unix).
pub fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
    std::fs::rename(source, target)
}

/// No upgrade needed on macOS: an open file can be unlinked directly.
pub fn open_for_retire(file: File) -> io::Result<File> {
    Ok(file)
}

/// Retire `file`: drop the handle and run the caller's remove. macOS
/// unlinks a mapped image immediately, so the plain remove is the whole
/// contract.
pub fn retire_open_file(file: File, plain_remove: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    drop(file);
    plain_remove()
}
