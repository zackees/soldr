//! macOS executable-naming implementation: names carry no suffix.

use std::path::{Path, PathBuf};

/// A bare tool name is already the native executable name on macOS.
pub fn native(name: &str) -> String {
    name.to_owned()
}

/// The path of `name` as a native executable beside the current one.
pub fn sibling(exe_dir: &Path, name: &str) -> PathBuf {
    exe_dir.join(name)
}


/// The suffix for wrapper scripts (`.cmd` on Windows, none elsewhere).
pub fn script_suffix() -> &'static str {
    ""
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_native_names_carry_no_suffix() {
        assert_eq!(native("soldr-daemon"), "soldr-daemon");
        assert_eq!(
            sibling(std::path::Path::new("/soldr/bin"), "soldr-daemon"),
            std::path::PathBuf::from("/soldr/bin/soldr-daemon")
        );
    }
}
