//! Windows executable lookup implementation (PATHEXT).

use std::ffi::OsStr;
use std::path::PathBuf;

/// The `PATHEXT` list (lowercased) with a default when the variable is
/// unset — the suffixes Windows considers executable without the caller
/// having to spell `.exe` (or `.cmd`, `.bat`, `.com`) explicitly.
pub fn candidate_extensions() -> Vec<String> {
    std::env::var_os("PATHEXT")
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

/// Look up `name` against a PATH-shaped value, trying each PATHEXT suffix.
pub fn find_on_path(name: &str, path_value: &OsStr) -> Option<PathBuf> {
    crate::platform::executable::search::find_on_path_using(
        name,
        path_value,
        &candidate_extensions(),
    )
}
