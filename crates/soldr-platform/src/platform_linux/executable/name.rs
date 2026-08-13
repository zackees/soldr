//! Linux executable-naming implementation: names carry no suffix.

use std::path::{Path, PathBuf};

/// A bare tool name is already the native executable name on Linux.
pub fn native(name: &str) -> String {
    name.to_owned()
}

/// The path of `name` as a native executable beside the current one.
pub fn sibling(exe_dir: &Path, name: &str) -> PathBuf {
    exe_dir.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] // allow-bare-test: soldr-platform is a dependency leaf; timed_test! lives in soldr-core (#2493)
    fn linux_native_names_carry_no_suffix() {
        assert_eq!(native("soldr-daemon"), "soldr-daemon");
        assert_eq!(
            sibling(std::path::Path::new("/soldr/bin"), "soldr-daemon"),
            std::path::PathBuf::from("/soldr/bin/soldr-daemon")
        );
    }
}
