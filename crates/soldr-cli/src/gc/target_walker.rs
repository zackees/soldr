//! Cross-repo `target/` walker for `soldr gc target` (issue #574).
//!
//! Walks a configurable root (default `~/dev`), finds every directory
//! containing a `Cargo.toml`, and sums each sibling `target/`'s size.
//! Returned entries are NOT sorted; sorting is the caller's
//! responsibility so the CLI handler can choose between size-desc
//! reports, JSON-stable paths, etc.
//!
//! Discovery (#681) is dual-path: a `target/` is reclaimable either
//! because a sibling `Cargo.toml` proves the parent is a cargo
//! workspace (the original "manifest" path), or because the contents
//! of `target/` itself look like cargo output (the new "content"
//! path — covers perf-test scratch trees and other layouts that lack
//! a manifest neighbor).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::walks::fast_directory_size_and_files;

/// How a `TargetEntry` was discovered. Surfaced in the human-readable
/// purge plan and JSON output so users can spot-check
/// content-discovered entries before accepting `--yes`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TargetDiscovery {
    /// `target/` was identified via a sibling `Cargo.toml`. The
    /// original (pre-#681) detection path. Treat as high-confidence:
    /// the parent declared itself a cargo workspace.
    Manifest,
    /// `target/` was identified by its own contents (`debug/`,
    /// `release/`, `doc/`, `.rustc_info.json`, or `CACHEDIR.TAG`).
    /// Covers perf-test scratch dirs and workspace fixtures whose
    /// driver lives elsewhere in the tree.
    Content,
}

/// One reclaimable workspace `target/` directory.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct TargetEntry {
    /// Workspace root — the directory containing `Cargo.toml` (for
    /// `Manifest` discovery) or the parent of `target/` (for `Content`
    /// discovery, where no manifest is present).
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
    /// Whether this entry was found via a sibling `Cargo.toml`
    /// (`Manifest`) or by inspecting `target/`'s contents (`Content`).
    pub(crate) discovery: TargetDiscovery,
}

/// Top-level markers inside a `target/` directory that signal it was
/// produced by cargo. Any one is enough — cargo lays down all of
/// these depending on what's been run.
///
/// `debug/`, `release/`, `doc/`: profile output directories.
/// `.rustc_info.json`: cargo's metadata stamp.
/// `CACHEDIR.TAG`: cargo writes this so other tools can opt out of
/// indexing the tree.
///
/// Maven and JVM tooling also use a `target/` dir, but they ship
/// `target/classes/` or `target/maven-archiver/`, not these markers —
/// so the heuristic stays Rust-specific.
const CARGO_TARGET_MARKERS: &[&str] = &[
    "debug",
    "release",
    "doc",
    ".rustc_info.json",
    "CACHEDIR.TAG",
];

/// Returns true when `target_dir` contains any cargo-shape marker.
/// Used by the content-discovery path (#681) so a `target/` without
/// a sibling `Cargo.toml` is still reclaimable if its contents prove
/// it was produced by cargo.
fn looks_like_cargo_target(target_dir: &Path) -> bool {
    CARGO_TARGET_MARKERS
        .iter()
        .any(|m| target_dir.join(m).exists())
}

fn collect_target_candidates(
    root: &Path,
    max_depth: usize,
) -> (Vec<std::fs::DirEntry>, Vec<PathBuf>) {
    let mut manifests = Vec::new();
    let mut orphan_targets = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = stack.pop() {
        if depth >= max_depth {
            continue;
        }
        let Ok(read_dir) = std::fs::read_dir(&directory) else {
            continue;
        };
        let entries: Vec<_> = read_dir.flatten().collect();
        let has_manifest = entries
            .iter()
            .any(|entry| entry.file_name() == concat!("Cargo", ".toml"));
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == concat!("Cargo", ".toml") {
                manifests.push(entry);
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            if name == "target" {
                if !has_manifest {
                    orphan_targets.push(entry.path());
                }
                continue;
            }
            if matches!(
                name.as_ref(),
                ".git" | ".hg" | ".svn" | ".jj" | "node_modules"
            ) {
                continue;
            }
            stack.push((entry.path(), depth + 1));
        }
    }
    (manifests, orphan_targets)
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
    let (manifest_entries, content_candidates) = collect_target_candidates(root, max_depth);

    // Shared collector for content-discovery candidates (#681). The
    // `process_read_dir` closure is `Fn` and may be invoked from
    // multiple jwalk worker threads — Arc<Mutex<...>> keeps the
    // append-only collection sound without restricting the walk's
    // parallelism in any meaningful way (push is O(1)).
    #[cfg(any())]
    let _walker = jwalk::WalkDir::new(root)
        .follow_links(false)
        .max_depth(max_depth)
        .skip_hidden(false)
        .process_read_dir(move |_depth, _path, _state, children| {
            // Content-discovery hook (#681): if this directory holds a
            // `target/` child but NO sibling `Cargo.toml`, peek at the
            // target candidate and record it. Marker validation happens after the
            // parallel walk has quiesced, avoiding filesystem-visibility
            // races while children are still being enumerated. Done before
            // pruning so we still see the `target/` child here. Skipped when a
            // sibling `Cargo.toml` is present — the manifest pass below
            // will pick that case up at higher confidence.
            let has_cargo_toml = children.iter().any(|c| {
                c.as_ref()
                    .ok()
                    .is_some_and(|e| e.file_name().to_string_lossy() == "Cargo.toml")
            });
            if !has_cargo_toml {
                for child in children.iter() {
                    let Ok(entry) = child else { continue };
                    if entry.file_name().to_string_lossy() != "target" {
                        continue;
                    }
                    // Do not trust jwalk's cached file type here. Under a
                    // heavily parallel walk it can transiently report an
                    // unknown/non-directory type even though the child path is
                    // a directory. The quiesced second pass validates the
                    // candidate by checking cargo markers and walking it.
                    // Candidate collection is performed deterministically after
                    // the parallel manifest walk.
                }
            }

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

    // Pass 1 — manifest discovery (the original behavior).
    let walker = manifest_entries.into_iter().map(Some);
    for entry in walker.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
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
            discovery: TargetDiscovery::Manifest,
        });
    }

    // Pass 2 — content discovery (#681). Dedup against the manifest
    // pass: a `target/` already surfaced as `Manifest` always wins,
    // because the parent declared itself a cargo workspace and we
    // trust that over the heuristic.
    let manifest_targets: std::collections::HashSet<PathBuf> =
        out.iter().map(|e| e.target_dir.clone()).collect();
    for target_dir in content_candidates {
        if manifest_targets.contains(&target_dir) {
            continue;
        }
        if !looks_like_cargo_target(&target_dir) {
            continue;
        }
        let (size_bytes, file_count) = fast_directory_size_and_files(&target_dir);
        if size_bytes == 0 && file_count == 0 {
            continue;
        }
        let last_modified_ms = directory_mtime_ms(&target_dir);
        let workspace_root = target_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| target_dir.clone());
        out.push(TargetEntry {
            workspace_root,
            target_dir,
            size_bytes,
            file_count,
            last_modified_ms,
            discovery: TargetDiscovery::Content,
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

    /// Seed a `target/` directory without a sibling `Cargo.toml`,
    /// laying down a cargo-shape marker so the content-discovery
    /// pass (#681) recognises it. Returns the parent of `target/`.
    fn seed_orphan_target(root: &Path, name: &str, marker: &str, bytes: usize) -> PathBuf {
        let parent = root.join(name);
        std::fs::create_dir_all(&parent).expect("create parent");
        let target = parent.join("target");
        std::fs::create_dir_all(&target).expect("create target/");
        // Drop the marker. For directory markers (debug/release/doc)
        // we create the dir AND a blob inside so size > 0. For file
        // markers (.rustc_info.json / CACHEDIR.TAG) we write the file.
        let marker_path = target.join(marker);
        if matches!(marker, "debug" | "release" | "doc") {
            std::fs::create_dir_all(&marker_path).expect("create marker dir");
            std::fs::write(marker_path.join("blob.bin"), vec![0u8; bytes])
                .expect("write marker blob");
        } else {
            std::fs::write(&marker_path, vec![0u8; bytes]).expect("write marker file");
        }
        parent
    }

    timed_test!(walk_content_discovers_target_without_sibling_cargo_toml, {
        // Issue #681: a `target/` dir whose contents look cargo-shaped
        // but lacks a sibling Cargo.toml must still be reclaimable.
        // Exercises each of the five recognized markers.
        for marker in [
            "debug",
            "release",
            "doc",
            ".rustc_info.json",
            "CACHEDIR.TAG",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path();
            let parent = seed_orphan_target(root, &format!("scratch-{marker}"), marker, 4096);

            let entries = walk(root, 8);
            let by_disco: Vec<(TargetDiscovery, &Path)> = entries
                .iter()
                .map(|e| (e.discovery, e.workspace_root.as_path()))
                .collect();
            assert_eq!(
                entries.len(),
                1,
                "expected one content-discovered entry for marker {marker:?}; got {by_disco:?}",
            );
            assert_eq!(
                entries[0].discovery,
                TargetDiscovery::Content,
                "marker {marker:?} should produce a Content entry",
            );
            assert_eq!(entries[0].workspace_root, parent);
        }
    });

    timed_test!(walk_content_skips_target_without_cargo_markers, {
        // Negative: a `target/` dir with NO cargo-shape markers (e.g.
        // a Maven-style `target/classes/` layout) must NOT be picked
        // up by content discovery. This protects the heuristic from
        // false positives in JVM trees that share the `target/` name.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let maven_like = root.join("java-project").join("target");
        std::fs::create_dir_all(maven_like.join("classes")).expect("create maven-like layout");
        std::fs::write(
            maven_like.join("classes").join("Main.class"),
            b"\xca\xfe\xba\xbe",
        )
        .expect("write fake class file");

        let entries = walk(root, 8);
        assert!(
            entries.is_empty(),
            "Maven-style target/ without cargo markers must NOT be picked up; got {entries:?}",
        );
    });

    timed_test!(walk_manifest_path_wins_over_content_path, {
        // When both detection paths fire on the same target/, the
        // manifest entry (high-confidence, parent declared itself a
        // cargo workspace) wins. We never emit both.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // `seed_workspace` lays down a Cargo.toml AND a target/debug/
        // blob — exactly the dual-eligible shape.
        let workspace = seed_workspace(root, "dual", 1024);

        let entries = walk(root, 8);
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one entry; got {entries:?}"
        );
        assert_eq!(entries[0].workspace_root, workspace);
        assert_eq!(
            entries[0].discovery,
            TargetDiscovery::Manifest,
            "dual-eligible target/ must report as Manifest, not Content",
        );
    });

    timed_test!(walk_emits_both_paths_when_distinct_targets, {
        // A root with one manifest-discovered AND one
        // content-discovered target/ must emit both entries, each
        // tagged with the correct discovery field.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let manifest_workspace = seed_workspace(root, "real-project", 2048);
        let orphan_parent = seed_orphan_target(root, "perf-scratch", "debug", 4096);

        let entries = walk(root, 8);
        assert_eq!(entries.len(), 2, "expected two entries; got {entries:?}");
        let manifest = entries
            .iter()
            .find(|e| e.workspace_root == manifest_workspace)
            .expect("manifest entry present");
        let content = entries
            .iter()
            .find(|e| e.workspace_root == orphan_parent)
            .expect("content entry present");
        assert_eq!(manifest.discovery, TargetDiscovery::Manifest);
        assert_eq!(content.discovery, TargetDiscovery::Content);
    });

    timed_test!(walk_content_path_serializes_lowercase_discovery_in_json, {
        // The `TargetDiscovery` enum carries `#[serde(rename_all =
        // "lowercase")]` so the JSON wire form is the stable
        // string contract the CLI handler depends on. Lock it in.
        let manifest = serde_json::to_string(&TargetDiscovery::Manifest).expect("serialize");
        let content = serde_json::to_string(&TargetDiscovery::Content).expect("serialize");
        assert_eq!(manifest, "\"manifest\"");
        assert_eq!(content, "\"content\"");
    });
}
