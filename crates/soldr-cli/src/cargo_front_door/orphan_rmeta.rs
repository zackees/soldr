//! Sweep orphan `.rmeta` files left behind by a failed cargo build.
//!
//! A non-zero cargo exit can leave an `.rmeta` with no companion `.rlib`
//! (rmeta emitted, then rustc aborted before the codegen pass) in
//! `target/[<triple>/]<profile>/deps/`. Subsequent invocations then fail with
//! `E0463: can't find crate`, because cargo passes `--extern X=orphan.rmeta`
//! to dependents and rustc cannot link an rmeta-only crate. See soldr#410.
//!
//! soldr#2996: this lived in the deleted `rust_plan` module and took a target
//! cache plan, so it only ever ran for callers who had opted into the target
//! cache -- i.e. never on a default build, which is precisely when a failed
//! build leaves orphans behind. It now takes the target directory the front
//! door already resolves, so the sweep works for everyone.

pub(crate) fn prune_orphan_rmetas_after_failed_build(target_root: &std::path::Path) -> usize {
    let mut total = 0usize;
    for deps_dir in find_deps_dirs(target_root, 3) {
        total = total.saturating_add(prune_orphan_rmetas_in_deps(&deps_dir));
    }
    if total > 0 {
        eprintln!(
            "soldr: pruned {total} orphan .rmeta file(s) under {} after failed cargo build (soldr#410)",
            target_root.display()
        );
    }
    total
}

/// Locate `deps/` subdirectories under `root` up to `max_depth` levels
/// deep (inclusive). Designed to find the cargo `target/[<triple>/]<profile>/deps/`
/// trees without descending into unrelated directories like `incremental/`,
/// `build/`, or `doc/`.
fn find_deps_dirs(root: &std::path::Path, max_depth: usize) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    walk_for_deps_dirs(root, max_depth, &mut out);
    out
}

fn walk_for_deps_dirs(
    dir: &std::path::Path,
    remaining_depth: usize,
    out: &mut Vec<std::path::PathBuf>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == "deps") {
            out.push(path.clone());
            // Do not descend further: cargo never nests another `deps/`
            // inside a `deps/`, and we want to keep the walk shallow.
            continue;
        }
        if remaining_depth > 0 {
            walk_for_deps_dirs(&path, remaining_depth - 1, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Pre-populated target/ restore guard (issue #480).
// ---------------------------------------------------------------------------
//
// When `cargo chef cook` (or any prior `soldr cargo build`) has populated
// `target/`, running `zccache rust-plan restore` on top of the existing tree
// can produce the failure mode described in #480: `restored_file_count: 0 /
// artifact_absent_from_restored_plan: 1`, followed by cargo failing because
// the expected `.rmeta` files aren't where it left them.
//
// This guard detects that case (cargo `.fingerprint/` dirs already on disk)
// and skips restore, letting cargo work with the existing target tree. The
// warm-restore short-circuit (#229) covers the in-job repeat case where the
// plan inputs hash matches; this guard covers the cross-context case where
// cook saved one plan and the consumer build computes a different inputs
// hash but reuses the same target/.

/// Env var to override the prepopulated-target restore guard. When set to a
/// truthy value (anything other than "", "0", "false", "no", "off") the
/// guard is bypassed and `rust-plan restore` runs even when the target tree
/// already contains cargo fingerprints. Provided as an escape hatch for users
/// who specifically want the old behavior.
pub(crate) const SOLDR_FORCE_RESTORE_ENV_VAR: &str = "SOLDR_RUST_PLAN_FORCE_RESTORE";

/// Hold the profile directory's cargo build lock (`<profile>/.cargo-lock`,
/// the parent of `deps/`) for the length of a prune, or `None` when another
/// cargo holds it (soldr#3442). A concurrent build's `.rmeta` has no `.rlib`
/// yet, so it looks exactly like an orphan; the lock is the only signal that
/// separates the two. A missing lock file means no cargo ever built here, so
/// there is nothing to protect.
fn lock_profile_dir_for_prune(deps_dir: &std::path::Path) -> Option<Option<std::fs::File>> {
    let lock_path = deps_dir.parent()?.join(".cargo-lock");
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
    else {
        return Some(None);
    };
    match file.try_lock() {
        Ok(()) => Some(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => None,
        // An unsupported or failing lock cannot vouch that no build is
        // running, so keep every file.
        Err(std::fs::TryLockError::Error(_)) => None,
    }
}

/// Delete `.rmeta` files in `deps_dir` that have no companion library, unless
/// another cargo build currently holds the profile's build lock.
fn prune_orphan_rmetas_in_deps(deps_dir: &std::path::Path) -> usize {
    let Some(_build_lock) = lock_profile_dir_for_prune(deps_dir) else {
        eprintln!(
            "soldr: skipping orphan .rmeta prune under {}: another cargo build holds its lock (soldr#3442)",
            deps_dir.display()
        );
        return 0;
    };
    let entries = match std::fs::read_dir(deps_dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut rmeta_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut companion_stems: std::collections::HashSet<std::ffi::OsString> =
        std::collections::HashSet::new();

    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
        else {
            continue;
        };
        let Some(stem) = path.file_stem() else {
            continue;
        };
        match ext.as_str() {
            "rmeta" => rmeta_paths.push(path.clone()),
            "rlib" | "so" | "dylib" | "dll" => {
                companion_stems.insert(stem.to_owned());
            }
            _ => {}
        }
    }

    let mut deleted = 0;
    for rmeta in rmeta_paths {
        let Some(stem) = rmeta.file_stem() else {
            continue;
        };
        if companion_stems.contains(stem) {
            continue;
        }
        match std::fs::remove_file(&rmeta) {
            Ok(()) => deleted += 1,
            Err(e) => eprintln!(
                "soldr warning: failed to prune orphan rmeta {}: {e}",
                rmeta.display()
            ),
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with_rmeta() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempfile::tempdir().expect("temp root");
        let profile = root.path().join("debug");
        let deps = profile.join("deps");
        std::fs::create_dir_all(&deps).expect("deps");
        let rmeta = deps.join("libinflight.rmeta");
        std::fs::write(&rmeta, b"in flight").expect("rmeta");
        (root, profile, rmeta)
    }

    #[test]
    fn prune_leaves_rmeta_alone_while_another_build_holds_the_lock() {
        let (root, profile, rmeta) = profile_with_rmeta();
        let other_build = std::fs::File::create(profile.join(".cargo-lock")).expect("lock file");
        other_build.lock().expect("other build takes the lock");

        assert_eq!(prune_orphan_rmetas_after_failed_build(root.path()), 0);
        assert!(rmeta.exists(), "a concurrent build's .rmeta must survive");

        drop(other_build);
        assert_eq!(prune_orphan_rmetas_after_failed_build(root.path()), 1);
        assert!(!rmeta.exists());
    }

    #[test]
    fn prune_proceeds_when_no_cargo_ever_created_the_lock() {
        let (root, _profile, rmeta) = profile_with_rmeta();
        assert_eq!(prune_orphan_rmetas_after_failed_build(root.path()), 1);
        assert!(!rmeta.exists());
    }
}
