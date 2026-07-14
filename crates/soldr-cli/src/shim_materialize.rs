//! Helpers for materializing multicall shim names from the main `soldr`
//! binary.
//!
//! Issue #1302 removes tiny sidecar shim executables from release
//! archives. Installers now create hardlinks to `soldr` under the desired
//! argv[0] name, falling back to byte-for-byte copies when hardlinks are
//! unavailable.

use crate::core::SoldrError;
use std::path::{Path, PathBuf};

pub(crate) const LINK_MODE_HARDLINK: &str = "hardlink";
pub(crate) const LINK_MODE_COPY: &str = "copy";
pub(crate) const LINK_MODE_HARDLINK_OR_COPY: &str = "hardlink-or-copy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MaterializeResult {
    pub created: bool,
    pub link_mode: &'static str,
}

/// Resolve the source `soldr` binary that should be used for installed
/// multicall shims. When self-relocation is active, prefer the original
/// executable path so long-lived shims do not point at a GC-managed runtime
/// copy.
pub(crate) fn soldr_binary_source() -> Result<PathBuf, SoldrError> {
    if let Some(original) = std::env::var_os(crate::self_relocate::ORIGINAL_EXE_ENV_VAR) {
        let path = PathBuf::from(original);
        if path.is_file() {
            return Ok(path);
        }
    }
    std::env::current_exe().map_err(SoldrError::from)
}

/// Install `target` as a hardlink to `source`, falling back to a copy.
/// Returns `created=false` when the existing target already has identical
/// bytes.
pub(crate) fn materialize_executable(
    source: &Path,
    target: &Path,
) -> Result<MaterializeResult, SoldrError> {
    if executable_matches(target, source)? {
        return Ok(MaterializeResult {
            created: false,
            link_mode: LINK_MODE_HARDLINK_OR_COPY,
        });
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(SoldrError::Io)?;
    }

    let tmp = tmp_path_for(target);
    let _ = std::fs::remove_file(&tmp);
    let link_mode = match std::fs::hard_link(source, &tmp) {
        Ok(()) => LINK_MODE_HARDLINK,
        Err(_) => {
            std::fs::copy(source, &tmp).map_err(SoldrError::Io)?;
            LINK_MODE_COPY
        }
    };

    let source_meta = std::fs::metadata(source).map_err(SoldrError::Io)?;
    let perms = source_meta.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = perms;
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms).map_err(SoldrError::Io)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::set_permissions(&tmp, perms).map_err(SoldrError::Io)?;
    }

    if link_mode == LINK_MODE_COPY {
        if let Ok(mtime) = source_meta.modified() {
            std::fs::File::options()
                .write(true)
                .open(&tmp)
                .and_then(|f| f.set_modified(mtime))
                .map_err(SoldrError::Io)?;
        }
    }

    // Concurrent first-use materialization can put several writers here.
    // Publish first so identical winners are normally observed without being
    // removed. For stale targets on Windows (where rename does not replace),
    // remove and retry; bounded rechecks tolerate another writer publishing or
    // removing an identical target between any two filesystem operations.
    const PUBLISH_ATTEMPTS: usize = 20;
    let mut last_error = None;
    for attempt in 0..PUBLISH_ATTEMPTS {
        match std::fs::rename(&tmp, target) {
            Ok(()) => {
                return Ok(MaterializeResult {
                    created: true,
                    link_mode,
                });
            }
            Err(err) => last_error = Some(err),
        }

        if executable_matches(target, source).unwrap_or(false) {
            let _ = std::fs::remove_file(&tmp);
            return Ok(MaterializeResult {
                created: false,
                link_mode: LINK_MODE_HARDLINK_OR_COPY,
            });
        }

        if let Err(err) = std::fs::remove_file(target) {
            if err.kind() != std::io::ErrorKind::NotFound {
                last_error = Some(err);
            }
        }
        if attempt + 1 < PUBLISH_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    if executable_matches(target, source).unwrap_or(false) {
        let _ = std::fs::remove_file(&tmp);
        return Ok(MaterializeResult {
            created: false,
            link_mode: LINK_MODE_HARDLINK_OR_COPY,
        });
    }
    let _ = std::fs::remove_file(&tmp);
    Err(SoldrError::Io(last_error.unwrap_or_else(|| {
        std::io::Error::other(format!(
            "failed to publish executable shim {}",
            target.display()
        ))
    })))
}

fn tmp_path_for(target: &Path) -> PathBuf {
    // Threads in one process can observe the same clock tick, especially on
    // macOS. A colliding fallback copy can otherwise truncate another
    // thread's hardlinked temp file (and therefore the source executable).
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let sequence = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let suffix = format!("tmp.{pid}-{nanos}-{sequence}");
    let mut path = target.as_os_str().to_os_string();
    path.push(".");
    path.push(suffix);
    PathBuf::from(path)
}

pub(crate) fn executable_matches(target: &Path, source: &Path) -> Result<bool, SoldrError> {
    let Ok(target_meta) = std::fs::metadata(target) else {
        return Ok(false);
    };
    let Ok(source_meta) = std::fs::metadata(source) else {
        return Ok(false);
    };
    if target_meta.len() != source_meta.len() {
        return Ok(false);
    }
    Ok(blake3_file(target)? == blake3_file(source)?)
}

fn blake3_file(path: &Path) -> Result<[u8; 32], SoldrError> {
    zccache::hash::hash_file(path)
        .map(|hash| *hash.as_bytes())
        .map_err(SoldrError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_source(tmp: &tempfile::TempDir, bytes: &[u8]) -> PathBuf {
        let source = tmp.path().join("soldr");
        std::fs::write(&source, bytes).unwrap();
        source
    }

    crate::timed_test!(materialize_executable_creates_matching_target, {
        let tmp = tempfile::tempdir().unwrap();
        let source = fake_source(&tmp, b"fake-soldr-v1");
        let target = tmp.path().join("cargo");
        let result = materialize_executable(&source, &target).unwrap();
        assert!(result.created);
        assert!([LINK_MODE_HARDLINK, LINK_MODE_COPY].contains(&result.link_mode));
        assert_eq!(std::fs::read(&target).unwrap(), b"fake-soldr-v1");
    });

    crate::timed_test!(materialize_executable_is_idempotent_for_matching_bytes, {
        let tmp = tempfile::tempdir().unwrap();
        let source = fake_source(&tmp, b"fake-soldr-v1");
        let target = tmp.path().join("rustc");
        assert!(materialize_executable(&source, &target).unwrap().created);
        let second = materialize_executable(&source, &target).unwrap();
        assert!(!second.created);
    });

    crate::timed_test!(
        materialize_executable_accepts_an_identical_concurrent_winner,
        {
            let tmp = tempfile::tempdir().unwrap();
            let source = fake_source(&tmp, b"fake-soldr-v1");
            let target = tmp.path().join("rustc");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
            let writers = (0..16)
                .map(|_| {
                    let source = source.clone();
                    let target = target.clone();
                    let barrier = std::sync::Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        materialize_executable(&source, &target)
                    })
                })
                .collect::<Vec<_>>();

            for writer in writers {
                writer
                    .join()
                    .expect("materialization writer panicked")
                    .expect("concurrent materialization failed");
            }
            assert_eq!(std::fs::read(target).unwrap(), b"fake-soldr-v1");
        }
    );

    crate::timed_test!(materialize_executable_replaces_different_bytes, {
        let tmp = tempfile::tempdir().unwrap();
        let source = fake_source(&tmp, b"fake-soldr-v1");
        let target = tmp.path().join("rustfmt");
        materialize_executable(&source, &target).unwrap();
        std::fs::remove_file(&source).unwrap();
        std::fs::write(&source, b"fake-soldr-v2").unwrap();
        let replaced = materialize_executable(&source, &target).unwrap();
        assert!(replaced.created);
        assert_eq!(std::fs::read(&target).unwrap(), b"fake-soldr-v2");
    });

    crate::timed_test!(temporary_paths_are_unique_within_one_process, {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("rustc");
        let mut paths = std::collections::HashSet::new();
        for _ in 0..1_024 {
            assert!(paths.insert(tmp_path_for(&target)));
        }
    });
}
