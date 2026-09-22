//! macOS open-file retirement.
//!
//! Atomic replacement now comes from `kernal_api::platform::fs::replacement`
//! (soldr#3297); see `crates/soldr-platform/src/platform/fs/replace.rs`.

use std::fs::File;
use std::io;

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
