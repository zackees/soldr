//! Windows executable-naming implementation.

use std::path::{Path, PathBuf};

/// Append the Windows executable suffix to a bare tool name.
pub fn native(name: &str) -> String {
    format!("{name}.exe")
}

/// The path of `name` as a native executable beside the current one.
pub fn sibling(exe_dir: &Path, name: &str) -> PathBuf {
    exe_dir.join(native(name))
}

/// The suffix for wrapper scripts (`.cmd` on Windows, none elsewhere).
pub fn script_suffix() -> &'static str {
    ".cmd"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] // allow-bare-test: soldr-platform is a dependency leaf; timed_test! lives in soldr-core (#2493)
    fn windows_native_names_carry_the_exe_suffix() {
        assert_eq!(native("soldr-daemon"), "soldr-daemon.exe");
        assert_eq!(
            sibling(std::path::Path::new("/soldr/bin"), "soldr-daemon"),
            std::path::PathBuf::from("/soldr/bin/soldr-daemon.exe")
        );
    }
}
