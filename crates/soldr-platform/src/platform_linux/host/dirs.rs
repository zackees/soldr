//! Linux host facts: directory discovery.

use std::path::PathBuf;

/// The current user's home directory (`HOME`).
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
