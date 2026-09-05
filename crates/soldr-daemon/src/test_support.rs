//! Test-only helpers that must not enable zccache's standalone CLI features.
//!
//! soldr#2899: `zccache/test-support` is declared as `test-support = []` but
//! its module body calls `tracing_subscriber::fmt()`, so it only compiles
//! when some *other* feature has already pulled `dep:tracing-subscriber` in —
//! which, before this change, was `cli`. Dropping `cli` therefore also has to
//! drop the dev-dependency. The two helpers soldr actually used from it are
//! four lines each, so they live here instead of keeping the whole CLI
//! feature alive to satisfy a transitive dependency the feature never
//! declared.

use std::path::PathBuf;

/// Locate an executable by name on `PATH`.
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        if std::path::Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

/// Return Cargo's configured compiler, falling back to a `PATH` lookup for
/// tests driven directly rather than through Cargo.
pub(crate) fn rustc_from_env_or_path() -> PathBuf {
    std::env::var_os("RUSTC")
        .map(PathBuf::from)
        .or_else(|| find_on_path("rustc"))
        .unwrap_or_else(|| PathBuf::from("rustc"))
}
