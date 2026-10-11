//! Concurrency-safe population of one install source-cache entry
//! (soldr#3689).
//!
//! A fill is two-phase: [`begin`] returns either a cache [`Fill::Hit`] or a
//! [`Slot`] to populate; [`Slot::publish`] makes the populated tree visible.

use std::path::{Path, PathBuf};

use crate::core::SoldrError;

use super::cache;

/// Outcome of [`begin`].
pub(crate) enum Fill {
    /// A complete entry already exists at this path.
    Hit(PathBuf),
    /// The caller must populate [`Slot::staging`] and then publish it.
    Fill(Slot),
}

/// An in-progress fill of one cache entry. Holds the entry's exclusive
/// cross-process lock until dropped, so no other acquirer can remove or
/// write the entry while this one populates its private staging dir.
pub(crate) struct Slot {
    cache_dir: PathBuf,
    staging: PathBuf,
    _lock: std::fs::File,
}

impl Slot {
    /// Private sibling directory the caller writes the source tree into.
    pub(crate) fn staging(&self) -> &Path {
        &self.staging
    }

    /// Atomically publish the populated tree as the complete cache entry.
    pub(crate) fn publish(self) -> Result<PathBuf, SoldrError> {
        cache::clear_partial(&self.staging)?;
        cache::touch_last_use(&self.staging);
        // Any leftover (stale) entry was removed in `begin` under this
        // same lock, so the rename target is free.
        std::fs::rename(&self.staging, &self.cache_dir)?;
        Ok(self.cache_dir.clone())
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // Best-effort cleanup of an unpublished staging dir (failed fill).
        if self.staging.exists() {
            let _ = std::fs::remove_dir_all(&self.staging);
        }
    }
}

/// Start filling `cache_dir`, or report a hit. Blocks while another
/// process holds the entry's lock; that process usually completes the
/// entry, which this call then reports as a [`Fill::Hit`].
pub(crate) fn begin(cache_dir: &Path) -> Result<Fill, SoldrError> {
    let (parent, name) = match (cache_dir.parent(), cache_dir.file_name()) {
        (Some(parent), Some(name)) => (parent, name.to_string_lossy().into_owned()),
        _ => {
            return Err(SoldrError::Other(format!(
                "install: invalid source cache path {}",
                cache_dir.display()
            )))
        }
    };
    let lock = crate::fetch::syslib_common::acquire_install_lock(parent, &name)?;

    if cache::is_complete(cache_dir) {
        cache::touch_last_use(cache_dir);
        return Ok(Fill::Hit(cache_dir.to_path_buf()));
    }
    // Under the lock nobody else is writing here: whatever exists is a
    // crashed/stale acquisition. Removal failures must surface.
    if cache_dir.exists() {
        std::fs::remove_dir_all(cache_dir)?;
    }
    let staging = parent.join(format!(".{name}.staging-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    // The `.partial` marker keeps the TTL sweep away from the staging dir.
    cache::mark_partial(&staging)?;
    Ok(Fill::Fill(Slot {
        cache_dir: cache_dir.to_path_buf(),
        staging,
        _lock: lock,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    const FILES: usize = 40;

    fn write_file(dir: &Path, i: usize) -> Result<(), SoldrError> {
        let p = dir.join("src").join(format!("f{i}.rs"));
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, format!("// {i}\n"))?;
        Ok(())
    }

    /// Fill `cache_dir` with the fixture tree. `pause` runs after the first
    /// `pause_at` files so a second process can race the first.
    fn acquire(
        cache_dir: &Path,
        pause_at: usize,
        pause: impl FnOnce(),
    ) -> Result<PathBuf, SoldrError> {
        match begin(cache_dir)? {
            Fill::Hit(dir) => Ok(dir),
            Fill::Fill(slot) => {
                for i in 0..pause_at {
                    write_file(slot.staging(), i)?;
                }
                pause();
                std::fs::write(slot.staging().join("Cargo.toml"), b"[package]\n")?;
                for i in pause_at..FILES {
                    write_file(slot.staging(), i)?;
                }
                slot.publish()
            }
        }
    }

    fn assert_complete_tree(dir: &Path) {
        assert!(cache::is_complete(dir), "{} not complete", dir.display());
        assert!(dir.join("Cargo.toml").is_file());
        for i in 0..FILES {
            let p = dir.join("src").join(format!("f{i}.rs"));
            assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("// {i}\n"));
        }
        let count = std::fs::read_dir(dir.join("src")).unwrap().count();
        assert_eq!(count, FILES, "unexpected extra files in tree");
    }

    /// Snapshot whether `dir` holds the full fixture tree right now.
    fn tree_is_complete(dir: &Path) -> bool {
        cache::is_complete(dir)
            && dir.join("Cargo.toml").is_file()
            && (0..FILES).all(|i| {
                std::fs::read_to_string(dir.join("src").join(format!("f{i}.rs"))).ok()
                    == Some(format!("// {i}\n"))
            })
    }

    #[test]
    fn concurrent_acquires_of_same_entry_both_yield_complete_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("host/owner/repo@abc");
        let (a_paused_tx, a_paused_rx) = mpsc::channel::<()>();
        let (b_paused_tx, b_paused_rx) = mpsc::channel::<()>();

        // A pauses halfway and waits (bounded) for B to reach its own
        // midpoint. With a correct lock B cannot start filling while A
        // holds the entry, so A resumes after the timeout.
        let a_dir = cache_dir.clone();
        let a = std::thread::spawn(move || {
            let dir = acquire(&a_dir, FILES / 2, || {
                a_paused_tx.send(()).unwrap();
                let _ = b_paused_rx.recv_timeout(Duration::from_millis(500));
            })?;
            // Checked before B can resume: what A published must be whole.
            Ok::<_, SoldrError>((tree_is_complete(&dir), dir))
        });
        a_paused_rx.recv().unwrap();
        let b_dir = cache_dir.clone();
        let b = std::thread::spawn(move || {
            acquire(&b_dir, 0, || {
                let _ = b_paused_tx.send(());
                std::thread::sleep(Duration::from_millis(300));
            })
        });

        let (a_complete, a) = a.join().unwrap().expect("first acquire must succeed");
        let b = b.join().unwrap().expect("second acquire must succeed");
        assert!(a_complete, "first acquire published an incomplete tree");
        assert_eq!(a, b);
        assert_complete_tree(&a);
        // No stray staging dirs left beside the entry.
        let siblings: Vec<_> = std::fs::read_dir(cache_dir.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| !n.to_string_lossy().ends_with(".lock"))
            .collect();
        assert_eq!(siblings.len(), 1, "{siblings:?}");
    }

    #[test]
    fn stale_partial_entry_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("repo@abc");
        cache::mark_partial(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("junk"), b"x").unwrap();
        let dir = acquire(&cache_dir, 0, || {}).unwrap();
        assert_complete_tree(&dir);
        assert!(!dir.join("junk").exists());
    }
}
