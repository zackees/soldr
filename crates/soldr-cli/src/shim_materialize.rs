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
    if is_current(target, source)? {
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

    let _ = std::fs::remove_file(target);
    std::fs::rename(&tmp, target).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        SoldrError::Io(err)
    })?;

    Ok(MaterializeResult {
        created: true,
        link_mode,
    })
}

fn tmp_path_for(target: &Path) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let suffix = format!("tmp.{pid}-{nanos}");
    let mut path = target.as_os_str().to_os_string();
    path.push(".");
    path.push(suffix);
    PathBuf::from(path)
}

fn is_current(target: &Path, source: &Path) -> Result<bool, SoldrError> {
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
}
