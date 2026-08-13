//! Linux executable lookup implementation (no suffix list).

use std::ffi::OsStr;
use std::path::PathBuf;

/// Linux has no PATHEXT-style implicit suffix list.
pub fn candidate_extensions() -> Vec<String> {
    Vec::new()
}

/// Look up `name` against a PATH-shaped value.
pub fn find_on_path(name: &str, path_value: &OsStr) -> Option<PathBuf> {
    crate::platform::executable::search::find_on_path_using(name, path_value, &[])
}
