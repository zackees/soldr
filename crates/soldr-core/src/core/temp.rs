//! The soldr scratch root: where temporary files and directories go.
//!
//! Rust's [`std::env::temp_dir`] resolves to `/tmp` on Linux, which on most
//! distributions is a `tmpfs` — i.e. RAM. soldr writes things there that have
//! no business being in RAM: toolchain tarballs tens of megabytes wide, and
//! whole synthetic cache roots during tests.
//!
//! Two further problems follow from using the OS temp dir directly:
//!
//! * **Nothing reclaims it.** A `temp_dir().join(...)` plus `create_dir_all`
//!   leaves the directory behind forever. Only [`tempfile::TempDir`] cleans up
//!   on drop, and it cleans up whichever root it was created in.
//! * **Renames stop being atomic.** A "write to temp, rename into the cache"
//!   sequence is only atomic while both ends live on one filesystem. With the
//!   cache on disk and temp on `tmpfs`, `rename(2)` fails with `EXDEV` and the
//!   caller silently degrades to a copy.
//!
//! So the scratch root defaults to `<root>/tmp` — a *sibling* of `<root>/cache`,
//! not a child of it. [`SOLDR_TMPDIR_ENV_VAR`] overrides it.
//!
//! Sibling rather than child is deliberate, and was learned the hard way. Being
//! on the same volume as the cache is what keeps temp->cache renames atomic, and
//! `<cache>/tmp` satisfies that too. But scratch *inside* the cache is scratch
//! the cache's own maintenance can see: auto-GC, purge tiers and cache walks all
//! traverse `<cache>/**`. Tests are the sharpest case — they build synthetic
//! `SOLDR_CACHE_DIR` roots in scratch, so `<cache>/tmp` nested every test cache
//! root inside the real one and let ambient maintenance reach in. A sibling
//! keeps the same filesystem, and therefore the same atomicity, while staying
//! outside everything that walks the cache.
//!
//! This module only *locates* the root. Callers create directories inside it
//! with `tempfile::Builder::new().tempdir_in(...)` so cleanup stays RAII —
//! `tempfile` is deliberately not a runtime dependency of this crate.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::paths::SoldrPaths;

/// Escape hatch: point soldr's scratch space anywhere you like.
///
/// Set this to keep large intermediates off a small volume, or to put them
/// back on `tmpfs` deliberately when you know the writes are small and want
/// the speed.
pub const SOLDR_TMPDIR_ENV_VAR: &str = "SOLDR_TMPDIR";

/// Scratch subdirectory of the soldr root, alongside `cache/` and `bin/`.
const TEMP_DIR_NAME: &str = "tmp";

/// Read [`SOLDR_TMPDIR_ENV_VAR`], treating an empty or whitespace-only value
/// as unset — an exported-but-blank variable means "I did not configure
/// this", not "use the current directory".
fn temp_root_override() -> Option<PathBuf> {
    let raw = std::env::var_os(SOLDR_TMPDIR_ENV_VAR)?;
    if raw.to_string_lossy().trim().is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

/// The scratch root for a known [`SoldrPaths`].
///
/// Prefer this over [`temp_root`] wherever the caller already holds a
/// `SoldrPaths`: it keeps scratch inside *that* root, which is what makes
/// tests with a synthetic `SOLDR_CACHE_DIR` self-contained.
pub fn temp_root_for(paths: &SoldrPaths) -> PathBuf {
    temp_root_override().unwrap_or_else(|| paths.root.join(TEMP_DIR_NAME))
}

/// The scratch root for the ambient environment.
///
/// Falls back to [`std::env::temp_dir`] only when no cache root can be
/// resolved at all (no `SOLDR_CACHE_DIR`, no home directory) — a degraded
/// environment where refusing to produce a path would be worse than using
/// the OS default.
///
/// **Resolved once per process.** The location depends on `SOLDR_CACHE_DIR`
/// / `HOME` / `USERPROFILE`, and this crate's own test suite mutates those
/// (see soldr#1663). Re-reading them on every call let the scratch root move
/// underneath a caller mid-run — a directory one test was still using could
/// be reclaimed as another test's leftovers. Production resolves its paths
/// once at startup, so caching costs nothing there and removes the hazard.
pub fn temp_root() -> PathBuf {
    static RESOLVED: OnceLock<PathBuf> = OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            if let Some(explicit) = temp_root_override() {
                return explicit;
            }
            match SoldrPaths::new() {
                Ok(paths) => paths.root.join(TEMP_DIR_NAME),
                Err(_) => std::env::temp_dir(),
            }
        })
        .clone()
}

/// [`temp_root`], created on disk and ready to receive entries.
///
/// If the directory cannot be created — a read-only or full volume — this
/// degrades to [`std::env::temp_dir`] rather than failing the caller. Scratch
/// space is a means, not an end: a build should not die because the preferred
/// scratch location was unavailable.
pub fn ensure_temp_root() -> PathBuf {
    let root = temp_root();
    match std::fs::create_dir_all(&root) {
        Ok(()) => root,
        Err(_) => std::env::temp_dir(),
    }
}

/// [`temp_root_for`], created on disk. Same degradation rule as
/// [`ensure_temp_root`].
pub fn ensure_temp_root_for(paths: &SoldrPaths) -> PathBuf {
    let root = temp_root_for(paths);
    match std::fs::create_dir_all(&root) {
        Ok(()) => root,
        Err(_) => std::env::temp_dir(),
    }
}

/// Replace an existing file with a directory at the same path.
///
/// Tests use this to park an unusable path in front of code that expects a
/// directory, then hand the real directory over and assert the retry
/// succeeds. The naive `remove_file` + `create_dir` pair is **not atomic on
/// Windows** and has no safe non-retrying form.
///
/// `DeleteFileW` does not remove a name. It marks the file delete-pending;
/// the name stays in its parent directory until the last open handle closes.
/// Anything holding the file open with `FILE_SHARE_DELETE` — which is exactly
/// how Defender's real-time scanner opens a file it is inspecting — keeps that
/// name alive after `remove_file` has already returned `Ok`. The immediately
/// following `create_dir` then collides with the surviving name and fails with
/// `ERROR_ALREADY_EXISTS` (183), reported as [`std::io::ErrorKind::AlreadyExists`].
///
/// So the removal is retried until the name is genuinely gone, bounded by
/// `deadline`. On Unix the first attempt always succeeds and the loop costs
/// nothing, so there is no `cfg` split.
///
/// See soldr#2714, where this failed a `.gitignore`-only PR on the
/// windows-msvc target-run lane.
/// A unique sibling name for [`replace_file_with_dir`] to park the displaced
/// file under. Unique per call, because two calls in one process must not
/// collide -- and because a previous call's displaced file may still be
/// delete-pending under its own name.
fn displaced_sibling(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".displaced-{}-{}", std::process::id(), n));
    path.with_file_name(name)
}

pub fn replace_file_with_dir(path: &Path, deadline: Duration) -> std::io::Result<()> {
    // Preferred route: rename the file out of the way. A rename frees the
    // name *atomically*, so `create_dir` below has nothing to wait for even
    // while a scanner still holds the file open -- the handle follows the
    // file to its new name, and delete-pending never applies to `path`.
    //
    // This is what makes the deadline stop mattering in practice. The retry
    // loop underneath is a bounded wait on a third party, and a third party
    // is not bounded: soldr#2714 timed out at 10s and failed a test that had
    // nothing to do with unlink timing. Anything that could hold the handle
    // open long enough to defeat the unlink is equally invisible to a rename.
    //
    // Rename needs the same DELETE access the unlink needed, so a holder that
    // permits one permits the other. If it fails anyway, fall through to the
    // historical unlink-and-retry path rather than failing outright.
    let displaced = displaced_sibling(path);
    match std::fs::rename(path, &displaced) {
        Ok(()) => {
            // Best effort: this name is ours and unique, so if it lingers
            // delete-pending it collides with nothing.
            let _ = std::fs::remove_file(&displaced);
            return std::fs::create_dir(path);
        }
        // Preserve the fail-fast contract: a path that is not there is a
        // caller error, not something to wait out.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(err),
        Err(_) => {}
    }

    std::fs::remove_file(path)?;
    let give_up_at = Instant::now() + deadline;
    loop {
        match std::fs::create_dir(path) {
            Ok(()) => return Ok(()),
            // The only recoverable case: the deleted name has not vanished
            // yet. Every other error is reported immediately -- a retry loop
            // that swallows, say, PermissionDenied would just stall until the
            // deadline and then blame the wrong thing.
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if Instant::now() >= give_up_at {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!(
                            concat!(
                                "{} was still delete-pending after {:?}; ",
                                "some process is holding the removed file open"
                            ),
                            path.display(),
                            deadline
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_a_file_with_a_dir_leaves_a_directory() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("blocking");
        std::fs::write(&path, "not a directory").expect("seed file");

        replace_file_with_dir(&path, Duration::from_secs(10)).expect("swap");

        assert!(path.is_dir(), "the path must end up a directory");
    }

    // soldr#2714 — the failing shape: something else holds the file open
    // while the swap runs. Rust's `File` opens with `FILE_SHARE_DELETE`, so
    // on Windows this reproduces exactly what Defender's scanner does: the
    // unlink succeeds, the *name* survives until the handle closes, and the
    // create_dir underneath collides with it.
    //
    // The deadline is deliberately tiny. The old unlink-first implementation
    // could only wait it out and then fail; renaming the file away frees the
    // name at once, so no wait is needed and the budget is never consulted.
    //
    // On Unix the swap always worked, so this asserts the behaviour is now
    // the same on both -- the point being that the test is portable while the
    // defect was not.
    #[test]
    fn an_open_handle_does_not_block_the_swap() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("held-open");
        std::fs::write(&path, "not a directory").expect("seed file");

        let held = std::fs::File::open(&path).expect("hold a handle open");

        replace_file_with_dir(&path, Duration::from_millis(50))
            .expect("an open handle must not prevent the swap");
        assert!(path.is_dir(), "the path must end up a directory");

        drop(held);
    }

    // The displaced file is an implementation detail and must not survive as
    // litter next to the directory it made room for.
    #[test]
    fn the_swap_leaves_no_sibling_behind() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("blocking");
        std::fs::write(&path, "not a directory").expect("seed file");

        replace_file_with_dir(&path, Duration::from_secs(10)).expect("swap");

        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read temp dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["blocking".to_string()],
            "only the swapped directory may remain"
        );
    }

    // The retry loop must not become a catch-all. Only AlreadyExists means
    // "the delete has not landed yet"; anything else has to surface at once
    // instead of stalling until the deadline and then blaming delete-pending.
    #[test]
    fn a_missing_file_fails_immediately_rather_than_looping() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("absent");

        let started = Instant::now();
        let err = replace_file_with_dir(&path, Duration::from_secs(30))
            .expect_err("removing a file that is not there must fail");

        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the error must be reported without waiting out the deadline"
        );
    }

    // Two properties that pull against each other, so one test each.
    //
    // Same volume as the cache, or temp->cache renames stop being atomic.
    #[test]
    fn scratch_shares_the_soldr_root_with_the_cache() {
        let paths = SoldrPaths::with_root(PathBuf::from("/synthetic/root"));
        let root = temp_root_for(&paths);
        assert!(
            root.starts_with(&paths.root),
            "scratch must live under the soldr root so temp->cache renames are \
             same-filesystem; got {} for root {}",
            root.display(),
            paths.root.display()
        );
    }

    // ...but NOT inside the cache, or the cache's own maintenance walks into it.
    // Regression guard: `<cache>/tmp` nested every test's synthetic
    // SOLDR_CACHE_DIR inside the real cache root, exposing it to ambient
    // auto-GC and purge tiers.
    #[test]
    fn scratch_is_a_sibling_of_the_cache_not_a_child() {
        let paths = SoldrPaths::with_root(PathBuf::from("/synthetic/root"));
        let root = temp_root_for(&paths);
        assert!(
            !root.starts_with(&paths.cache),
            "scratch must NOT live under the cache -- auto-GC, purge tiers and \
             cache walks all traverse <cache>/**, and would reach into scratch \
             and into any synthetic cache root a test built there; got {} for \
             cache {}",
            root.display(),
            paths.cache.display()
        );
    }

    #[test]
    fn scratch_is_not_the_os_temp_dir_by_default() {
        let paths = SoldrPaths::with_root(PathBuf::from("/synthetic/root"));
        assert_ne!(
            temp_root_for(&paths),
            std::env::temp_dir(),
            "the OS temp dir is tmpfs on most Linux hosts; that is what this \
             module exists to avoid"
        );
    }
}
