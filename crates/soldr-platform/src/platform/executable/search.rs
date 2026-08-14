//! Executable lookup against a PATH-shaped value.

use std::path::PathBuf;

pub use crate::platform_imp::executable::search::{candidate_extensions, find_on_path};

/// Shared cfg-free walker: try `name` bare, then with every `extensions`
/// suffix, in each directory of `path_value`, in order. Names that already
/// contain a path separator are trusted as explicit paths and passed
/// through, so the walker never rewrites a caller's explicit `./tool` or
/// `bin\tool`.
///
/// Called by the concrete implementations with their platform's extension
/// list; kept here so Linux and macOS share one lookup loop.
pub(crate) fn find_on_path_using(
    name: &str,
    path_value: &std::ffi::OsStr,
    extensions: &[String],
) -> Option<PathBuf> {
    if name.contains('/') || name.contains('\\') {
        return Some(PathBuf::from(name));
    }
    for dir in std::env::split_paths(path_value) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        for extension in extensions {
            let candidate = dir.join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_walker_tries_bare_then_extensions_in_each_dir() {
        let temp =
            std::env::temp_dir().join(format!("soldr-platform-search-{}", std::process::id()));
        let first = temp.join("first");
        let second = temp.join("second");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        // Bare name wins even when an extension match exists earlier.
        std::fs::write(first.join("tool"), b"").unwrap();
        std::fs::write(second.join("tool.cmd"), b"").unwrap();
        let path_value = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(
            find_on_path_using(
                "tool",
                &path_value,
                &[".cmd".to_string(), ".exe".to_string()]
            ),
            Some(first.join("tool"))
        );

        // Extension match in a later dir is found when bare is absent.
        std::fs::remove_file(first.join("tool")).unwrap();
        assert_eq!(
            find_on_path_using(
                "tool",
                &path_value,
                &[".cmd".to_string(), ".exe".to_string()]
            ),
            Some(second.join("tool.cmd"))
        );
        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn shared_walker_trusts_explicit_paths() {
        // Path-like arguments are never rewritten into a lookup.
        assert_eq!(
            find_on_path_using("./tool", std::ffi::OsStr::new(""), &[]),
            Some(std::path::PathBuf::from("./tool"))
        );
        assert_eq!(
            find_on_path_using("bin\\tool", std::ffi::OsStr::new(""), &[]),
            Some(std::path::PathBuf::from("bin\\tool"))
        );
    }
}
