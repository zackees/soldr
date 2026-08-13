//! Windows host facts: directory discovery.

use std::path::PathBuf;

/// The current user's home directory (`USERPROFILE`).
pub fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}
