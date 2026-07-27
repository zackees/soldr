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
//! So the scratch root defaults to `<cache>/tmp`, which is on the same volume
//! as the cache by construction. [`SOLDR_TMPDIR_ENV_VAR`] overrides it.
//!
//! This module only *locates* the root. Callers create directories inside it
//! with `tempfile::Builder::new().tempdir_in(...)` so cleanup stays RAII —
//! `tempfile` is deliberately not a runtime dependency of this crate.

use std::path::PathBuf;
use std::sync::OnceLock;

use super::paths::SoldrPaths;

/// Escape hatch: point soldr's scratch space anywhere you like.
///
/// Set this to keep large intermediates off a small volume, or to put them
/// back on `tmpfs` deliberately when you know the writes are small and want
/// the speed.
pub const SOLDR_TMPDIR_ENV_VAR: &str = "SOLDR_TMPDIR";

/// Scratch subdirectory of the cache root.
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
    temp_root_override().unwrap_or_else(|| paths.cache.join(TEMP_DIR_NAME))
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
                Ok(paths) => paths.cache.join(TEMP_DIR_NAME),
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

#[cfg(test)]
mod tests {
    use super::*;

    // Guards the property the whole module exists for: scratch must share a
    // filesystem with the cache so `rename` into the cache stays atomic.
    crate::timed_test!(scratch_defaults_inside_the_cache_root, {
        let paths = SoldrPaths::with_root(PathBuf::from("/synthetic/root"));
        let root = temp_root_for(&paths);
        assert!(
            root.starts_with(&paths.cache),
            "scratch must live under the cache root so temp->cache renames are \
             same-filesystem; got {} for cache {}",
            root.display(),
            paths.cache.display()
        );
    });

    crate::timed_test!(scratch_is_not_the_os_temp_dir_by_default, {
        let paths = SoldrPaths::with_root(PathBuf::from("/synthetic/root"));
        assert_ne!(
            temp_root_for(&paths),
            std::env::temp_dir(),
            "the OS temp dir is tmpfs on most Linux hosts; that is what this \
             module exists to avoid"
        );
    });
}
