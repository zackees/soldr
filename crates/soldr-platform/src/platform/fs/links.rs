//! Link/reparse classification, symlink creation/removal, and archive
//! link materialization.

use std::path::{Component, Path, PathBuf};

pub use crate::platform_imp::fs::links::{
    create, hard_link_count, is_link_or_reparse, remove, unpack_archive_entries,
};

/// Lexically resolve a symlink `target` (as stored in an archive,
/// `/`-separated and usually relative) against the link's parent
/// directory, refusing absolute targets and any result that escapes
/// `dest`. Returns the absolute on-disk path the link points at.
///
/// Used by the Windows replay path to decide the NTFS link flavor and as
/// the source of the copy fallback — it deliberately never resolves
/// outside the extraction root, so a hostile archive cannot make the
/// copy fallback read arbitrary host files. Kept cfg-free so the
/// containment logic is compiled and verified on every host.
pub fn resolve_link_target(dest: &Path, link_path: &Path, target: &Path) -> Option<PathBuf> {
    let link_rel = link_path.strip_prefix(dest).ok()?;
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    for component in link_rel.components() {
        match component {
            Component::Normal(part) => stack.push(part.to_os_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    // Drop the link's own file name; targets are relative to its parent.
    stack.pop()?;
    for component in target.components() {
        match component {
            Component::Normal(part) => stack.push(part.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                stack.pop()?;
            }
            // Absolute targets (RootDir / Prefix) never resolve inside dest.
            _ => return None,
        }
    }
    let mut resolved = dest.to_path_buf();
    for part in stack {
        resolved.push(part);
    }
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] // allow-bare-test: soldr-platform is a dependency leaf; timed_test! lives in soldr-core (#2493)
    fn relative_targets_resolve_inside_the_root() {
        let dest = Path::new("/sdk");
        let link = Path::new("/sdk/usr/lib");
        // `usr/lib -> lib64` (same parent) and `usr/lib -> ../lib64`
        // (up one, back inside the root).
        assert_eq!(
            resolve_link_target(dest, link, Path::new("lib64")),
            Some(PathBuf::from("/sdk/usr/lib64"))
        );
        assert_eq!(
            resolve_link_target(dest, link, Path::new("../lib64")),
            Some(PathBuf::from("/sdk/lib64"))
        );
    }

    #[test] // allow-bare-test: soldr-platform is a dependency leaf; timed_test! lives in soldr-core (#2493)
    fn absolute_and_escaping_targets_never_resolve() {
        let dest = Path::new("/sdk");
        let link = Path::new("/sdk/usr/lib");
        assert_eq!(
            resolve_link_target(dest, link, Path::new("/etc/passwd")),
            None
        );
        // Two ups from usr/ is already outside the root.
        assert_eq!(
            resolve_link_target(dest, link, Path::new("../../lib64")),
            None
        );
        assert_eq!(
            resolve_link_target(dest, link, Path::new("../../../etc/passwd")),
            None
        );
    }
}
