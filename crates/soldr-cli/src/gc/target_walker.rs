//! Cross-repo `target/` walker for `soldr gc target` (issue #574).
//!
//! Walks a configurable root (default `~/dev`), finds every directory
//! containing a `Cargo.toml`, and sums each sibling `target/`'s size.
//! Returned entries are NOT sorted; sorting is the caller's
//! responsibility so the CLI handler can choose between size-desc
//! reports, JSON-stable paths, etc.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use jwalk::WalkDir;
use serde::Serialize;

use super::walks::fast_directory_size_and_files;

/// One reclaimable workspace `target/` directory.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct TargetEntry {
    /// Workspace root — the directory containing `Cargo.toml`.
    pub(crate) workspace_root: PathBuf,
    /// Sibling `target/` directory we'd actually delete.
    pub(crate) target_dir: PathBuf,
    /// Total bytes on disk under `target_dir`.
    pub(crate) size_bytes: u64,
    /// Files counted under `target_dir`.
    pub(crate) file_count: u64,
    /// Unix-ms mtime of `target_dir` (most-recently-modified entry
    /// at the top level). Zero when unavailable.
    pub(crate) last_modified_ms: i64,
}

/// Walk `root` up to `max_depth` directories deep, collecting every
/// workspace that has a sibling `target/` directory. VCS metadata
/// (`.git/`, `.hg/`, `.svn/`, `.jj/`) and `node_modules/` are pruned;
/// other dot-prefixed dirs (notably `.claude/worktrees/` where clud's
/// `/clud-pr` skill puts feature-branch workspaces) are descended into
/// because they routinely contain real Rust build artifacts that
/// `soldr gc target --purge` should be able to reclaim. Symlinks are
/// not followed.
///
/// Issue #680: the previous `skip_hidden(true)` filter was too
/// coarse — it conflated "VCS metadata" (skip) with "tool sandboxes
/// that happen to be dot-prefixed" (don't skip). On the reporter's box
/// that flipped 9.1 GB of reclaimable space invisible to `gc target`.
pub(crate) fn walk(root: &Path, max_depth: usize) -> Vec<TargetEntry> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut out: Vec<TargetEntry> = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .max_depth(max_depth)
        .skip_hidden(false)
        .process_read_dir(|_depth, _path, _state, children| {
            // Why: never descend into a `target/` we're about to report
            // on — saves a huge amount of recursive stat work on big
            // workspaces and avoids surfacing nested workspace target
            // dirs as their own entries.
            //
            // Why the explicit deny-list instead of `skip_hidden(true)`:
            // see #680. We still want to skip VCS metadata roots and
            // `node_modules` (gigantic, never holds Rust `target/`), but
            // dot-prefixed tool sandboxes like `.claude/worktrees/` DO
            // hold real Rust workspaces and must be walked.
            children.retain(|entry_result| {
                let Ok(entry) = entry_result else {
                    return true;
                };
                let name = entry.file_name().to_string_lossy().to_string();
                if name == "target" {
                    return false;
                }
                !matches!(
                    name.as_str(),
                    ".git" | ".hg" | ".svn" | ".jj" | "node_modules"
                )
            });
        });

    for entry in walker.into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.file_name().to_string_lossy() != "Cargo.toml" {
            continue;
        }
        let manifest_path = entry.path();
        let Some(workspace_root) = manifest_path.parent().map(|p| p.to_path_buf()) else {
            continue;
        };
        let target_dir = workspace_root.join("target");
        if !target_dir.is_dir() {
            continue;
        }
        let (size_bytes, file_count) = fast_directory_size_and_files(&target_dir);
        if size_bytes == 0 && file_count == 0 {
            continue;
        }
        let last_modified_ms = directory_mtime_ms(&target_dir);
        out.push(TargetEntry {
            workspace_root,
            target_dir,
            size_bytes,
            file_count,
            last_modified_ms,
        });
    }
    out
}

fn directory_mtime_ms(path: &Path) -> i64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(_) => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timed_test;

    fn seed_workspace(root: &Path, name: &str, target_bytes: usize) -> PathBuf {
        let workspace = root.join(name);
        std::fs::create_dir_all(&workspace).expect("create workspace dir");
        std::fs::write(workspace.join("Cargo.toml"), b"[package]\nname=\"x\"\n")
            .expect("write Cargo.toml");
        let target = workspace.join("target");
        std::fs::create_dir_all(target.join("debug")).expect("create target/debug");
        std::fs::write(
            target.join("debug").join("blob.bin"),
            vec![0u8; target_bytes],
        )
        .expect("write blob");
        workspace
    }

    timed_test!(walk_finds_workspaces_and_returns_unsorted, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let small = seed_workspace(root, "small", 1024);
        let large = seed_workspace(root, "large", 1024 * 1024);
        let medium = seed_workspace(root, "medium", 64 * 1024);

        let mut entries = walk(root, 4);
        entries.sort_by_key(|e| e.size_bytes);

        let paths: Vec<&Path> = entries.iter().map(|e| e.workspace_root.as_path()).collect();
        assert!(paths.contains(&small.as_path()), "found {paths:?}");
        assert!(paths.contains(&medium.as_path()), "found {paths:?}");
        assert!(paths.contains(&large.as_path()), "found {paths:?}");

        let large_entry = entries
            .iter()
            .find(|e| e.workspace_root == large)
            .expect("large workspace present");
        assert!(large_entry.size_bytes >= 1024 * 1024);
        assert!(large_entry.target_dir.ends_with("target"));
        assert!(large_entry.file_count >= 1);
    });

    timed_test!(walk_descends_into_dot_claude_worktrees, {
        // Issue #680: clud's `/clud-pr` skill stores every PR's
        // feature-branch worktree under `.claude/worktrees/<branch>/`
        // and builds Rust there. The walker MUST descend into
        // `.claude/` so those targets are reclaimable.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let worktree_parent = root
            .join(".claude")
            .join("worktrees")
            .join("feat-some-branch");
        std::fs::create_dir_all(&worktree_parent).expect("create worktree parent");
        let workspace = seed_workspace(&worktree_parent, "ws", 4096);

        let entries = walk(root, 8);
        let paths: Vec<&Path> = entries.iter().map(|e| e.workspace_root.as_path()).collect();
        assert!(
            paths.contains(&workspace.as_path()),
            "expected `.claude/worktrees/...` target to be discovered; got {paths:?}",
        );
    });

    timed_test!(walk_still_skips_vcs_metadata_dirs, {
        // Issue #680: the relaxed hidden-dir filter must NOT cause
        // VCS metadata roots (`.git/`, `.hg/`, `.svn/`, `.jj/`) or
        // `node_modules/` to be walked. Cargo-shaped layouts can show
        // up inside `.git/modules/<sub>/` (git submodules), inside
        // checked-in fixtures, etc. — none of those are user-managed
        // build artifacts and must stay invisible to `gc target`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        for vcs in [".git", ".hg", ".svn", ".jj", "node_modules"] {
            let _seeded = seed_workspace(&root.join(vcs), "ws", 2048);
        }
        // And one positive-control workspace so we know walking happened.
        let real = seed_workspace(root, "real-project", 1024);

        let entries = walk(root, 8);
        let paths: Vec<&Path> = entries.iter().map(|e| e.workspace_root.as_path()).collect();
        assert_eq!(
            entries.len(),
            1,
            "expected only the real workspace; got {paths:?}",
        );
        assert!(paths.contains(&real.as_path()));
    });

    timed_test!(walk_does_not_recurse_into_target_dirs, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let parent = seed_workspace(root, "outer", 1024);

        // Nested Cargo.toml *inside* the outer target/ — should be
        // ignored because we prune `target/` from the walk.
        let nested = parent.join("target").join("nested-project");
        std::fs::create_dir_all(&nested).expect("nested project dir");
        std::fs::write(nested.join("Cargo.toml"), b"[package]\nname=\"y\"\n")
            .expect("write nested Cargo.toml");
        std::fs::create_dir_all(nested.join("target")).expect("nested target/");
        std::fs::write(nested.join("target").join("blob.bin"), vec![0u8; 1024])
            .expect("write nested blob");

        let entries = walk(root, 8);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].workspace_root.ends_with("outer"));
    });

    timed_test!(walk_respects_max_depth, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let deep = root.join("a").join("b").join("c").join("project");
        std::fs::create_dir_all(&deep).expect("deep nested");
        std::fs::write(deep.join("Cargo.toml"), b"[package]\nname=\"z\"\n")
            .expect("write deep Cargo.toml");
        std::fs::create_dir_all(deep.join("target")).expect("deep target/");
        std::fs::write(deep.join("target").join("blob"), vec![0u8; 1024]).expect("write deep blob");

        // a/b/c/project/Cargo.toml is 5 levels under root — depth 4
        // misses it, depth 8 finds it.
        assert!(walk(root, 4).is_empty());
        assert_eq!(walk(root, 8).len(), 1);
    });

    timed_test!(walk_skips_workspaces_without_target_dir, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let no_target = root.join("no-target");
        std::fs::create_dir_all(&no_target).expect("create dir");
        std::fs::write(no_target.join("Cargo.toml"), b"[package]\nname=\"w\"\n")
            .expect("write Cargo.toml");

        let entries = walk(root, 4);
        assert!(entries.is_empty());
    });

    timed_test!(walk_returns_empty_when_root_missing, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bogus = tmp.path().join("does-not-exist");
        let entries = walk(&bogus, 4);
        assert!(entries.is_empty());
    });
}
