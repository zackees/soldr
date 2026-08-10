//! CacheManifest persistence and central-registry helpers.
//!
//! Phase 2 of #228 (#231). The broker and standalone cleanup tool both
//! use this module. Manifests are prost-encoded protobuf and carry a
//! `self_sha256` digest over the encoded manifest with that field
//! cleared.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(not(windows))]
use std::fs::File;

use prost::Message;
use sha2::{Digest, Sha256};

use crate::broker::host_identity;
use crate::broker::lifecycle::names::{validate_service_name, validate_version, PipePathError};
use crate::broker::protocol::{CacheManifest, HostIdentity};
use crate::broker::secure_dir;

/// Filename written inside each daemon cache root.
pub const ROOT_MANIFEST_FILE: &str = ".running-process-manifest.pb";

/// Stable v1 manifest media type.
pub const CACHE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.running-process.cache-manifest.v1";

/// Highest manifest schema this crate understands.
pub const SUPPORTED_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Test/development override for the central manifest registry directory.
pub const RUNNING_PROCESS_MANIFEST_DIR_ENV: &str = "RUNNING_PROCESS_MANIFEST_DIR";

/// Errors returned by manifest persistence and validation.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// Filesystem operation failed.
    #[error("manifest I/O failed: {0}")]
    Io(#[from] io::Error),
    /// Protobuf decode failed.
    #[error("manifest protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
    /// Protobuf encode failed.
    #[error("manifest protobuf encode failed: {0}")]
    Encode(#[from] prost::EncodeError),
    /// The manifest's self_sha256 digest did not match its content.
    #[error("manifest self_sha256 mismatch")]
    Corruption,
    /// The manifest uses a newer schema version than this crate supports.
    #[error("manifest schema too new: got {got}, supported {supported}")]
    SchemaTooNew {
        /// Manifest schema version read from disk.
        got: u32,
        /// Maximum schema version this crate can read.
        supported: u32,
    },
    /// Service/version validation failed while deriving a registry path.
    #[error(transparent)]
    InvalidName(#[from] PipePathError),
    /// A path had no parent directory.
    #[error("manifest path has no parent: {0}")]
    MissingParent(PathBuf),
    /// Central-registry permissions are too broad.
    #[error("central manifest registry has insecure permissions: {0}")]
    InsecureRegistry(PathBuf),
}

/// Result of scanning one central-registry entry.
#[derive(Debug)]
pub struct ManifestScanEntry {
    /// Full path to the manifest file.
    pub path: PathBuf,
    /// Read result for that path.
    pub result: Result<CacheManifest, ManifestError>,
}

/// Write `<cache_root>/.running-process-manifest.pb` atomically.
pub fn write_to_root(cache_root: &Path, manifest: &CacheManifest) -> Result<(), ManifestError> {
    fs::create_dir_all(cache_root)?;
    secure_dir::ensure_private_dir(cache_root)?;
    let target = cache_root.join(ROOT_MANIFEST_FILE);
    write_manifest_file(&target, manifest)
}

/// Write `<central_registry>/{service}-{version}.pb` atomically.
pub fn write_to_central(
    service_name: &str,
    version: &str,
    manifest: &CacheManifest,
) -> Result<PathBuf, ManifestError> {
    let dir = central_registry_dir();
    write_to_central_in_dir(&dir, service_name, version, manifest)
}

/// Testable variant of [`write_to_central`] with an explicit registry dir.
pub fn write_to_central_in_dir(
    registry_dir: &Path,
    service_name: &str,
    version: &str,
    manifest: &CacheManifest,
) -> Result<PathBuf, ManifestError> {
    ensure_central_registry_dir(registry_dir)?;
    let target = central_manifest_path(registry_dir, service_name, version)?;
    write_manifest_file(&target, manifest)?;
    Ok(target)
}

/// Read and integrity-verify a CacheManifest.
pub fn read_manifest(path: &Path) -> Result<CacheManifest, ManifestError> {
    let bytes = fs::read(path)?;
    let manifest = CacheManifest::decode(bytes.as_slice())?;
    verify_schema(&manifest)?;
    verify_self_sha256(&manifest)?;
    Ok(manifest)
}

/// Enumerate parseable manifests for this host and boot.
///
/// Corrupt or stale manifests are skipped. Use [`scan_central`] when
/// callers need error details.
pub fn enumerate_central(registry_dir: &Path) -> Vec<CacheManifest> {
    let current_host = host_identity::current();
    enumerate_central_for_host(registry_dir, &current_host)
}

/// Testable variant of [`enumerate_central`] with an explicit current host.
pub fn enumerate_central_for_host(
    registry_dir: &Path,
    current_host: &HostIdentity,
) -> Vec<CacheManifest> {
    scan_central(registry_dir)
        .into_iter()
        .filter_map(|entry| match entry.result {
            Ok(manifest) if manifest_matches_host(&manifest, current_host) => Some(manifest),
            _ => None,
        })
        .collect()
}

/// Scan every `.pb` file in a registry and keep per-file errors.
pub fn scan_central(registry_dir: &Path) -> Vec<ManifestScanEntry> {
    match secure_dir::private_dir_permissions_are_private(registry_dir) {
        Ok(true) => {}
        Ok(false) => {
            return vec![ManifestScanEntry {
                path: registry_dir.to_path_buf(),
                result: Err(ManifestError::InsecureRegistry(registry_dir.to_path_buf())),
            }];
        }
        Err(_) if !registry_dir.exists() => return Vec::new(),
        Err(err) => {
            return vec![ManifestScanEntry {
                path: registry_dir.to_path_buf(),
                result: Err(ManifestError::Io(err)),
            }];
        }
    }

    let read_dir = match fs::read_dir(registry_dir) {
        Ok(read_dir) => read_dir,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("pb") {
            continue;
        }
        let result = read_manifest(&path);
        out.push(ManifestScanEntry { path, result });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Return the platform central-registry directory.
///
/// `RUNNING_PROCESS_MANIFEST_DIR` is honored as a test/development
/// override. Production callers should leave it unset.
pub fn central_registry_dir() -> PathBuf {
    if let Some(path) = std::env::var_os(RUNNING_PROCESS_MANIFEST_DIR_ENV) {
        return PathBuf::from(path);
    }

    #[cfg(windows)]
    {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("running-process")
            .join("manifests")
    }
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Library")
            .join("Application Support")
            .join("running-process")
            .join("manifests")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            PathBuf::from(data_home)
                .join("running-process")
                .join("manifests")
        } else {
            dirs::home_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join(".local")
                .join("share")
                .join("running-process")
                .join("manifests")
        }
    }
}

/// Ensure the central-registry directory exists with private permissions.
pub fn ensure_central_registry_dir(path: &Path) -> Result<(), ManifestError> {
    secure_dir::ensure_private_dir(path)?;
    if !secure_dir::private_dir_permissions_are_private(path)? {
        return Err(ManifestError::InsecureRegistry(path.to_path_buf()));
    }
    Ok(())
}

/// Compute the central-registry path for one service/version manifest.
pub fn central_manifest_path(
    registry_dir: &Path,
    service_name: &str,
    version: &str,
) -> Result<PathBuf, ManifestError> {
    validate_service_name(service_name)?;
    validate_version(version)?;
    Ok(registry_dir.join(format!("{service_name}-{version}.pb")))
}

/// Clone `manifest`, fill schema/media/hash fields, and return the copy.
pub fn manifest_with_self_sha256(manifest: &CacheManifest) -> Result<CacheManifest, ManifestError> {
    let mut out = manifest.clone();
    out.manifest_schema_version = SUPPORTED_MANIFEST_SCHEMA_VERSION;
    if out.media_type.is_empty() {
        out.media_type = CACHE_MANIFEST_MEDIA_TYPE.to_string();
    }
    out.self_sha256.clear();
    let digest = sha256_for_manifest(&out)?;
    out.self_sha256 = digest.to_vec();
    Ok(out)
}

/// Compute the SHA-256 digest with `self_sha256` cleared.
pub fn sha256_for_manifest(manifest: &CacheManifest) -> Result<[u8; 32], ManifestError> {
    let mut clone = manifest.clone();
    clone.self_sha256.clear();
    let mut bytes = Vec::new();
    clone.encode(&mut bytes)?;
    let digest = Sha256::digest(&bytes);
    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

fn write_manifest_file(path: &Path, manifest: &CacheManifest) -> Result<(), ManifestError> {
    let manifest = manifest_with_self_sha256(manifest)?;
    let mut bytes = Vec::new();
    manifest.encode(&mut bytes)?;
    atomic_write(path, &bytes)
}

fn verify_schema(manifest: &CacheManifest) -> Result<(), ManifestError> {
    if manifest.manifest_schema_version > SUPPORTED_MANIFEST_SCHEMA_VERSION {
        return Err(ManifestError::SchemaTooNew {
            got: manifest.manifest_schema_version,
            supported: SUPPORTED_MANIFEST_SCHEMA_VERSION,
        });
    }
    Ok(())
}

fn verify_self_sha256(manifest: &CacheManifest) -> Result<(), ManifestError> {
    if manifest.self_sha256.len() != 32 {
        return Err(ManifestError::Corruption);
    }
    let expected = sha256_for_manifest(manifest)?;
    if manifest.self_sha256.as_slice() != expected {
        return Err(ManifestError::Corruption);
    }
    Ok(())
}

fn manifest_matches_host(manifest: &CacheManifest, current_host: &HostIdentity) -> bool {
    let Some(host) = manifest.host.as_ref() else {
        return true;
    };
    (host.machine_id.is_empty() || host.machine_id == current_host.machine_id)
        && (host.boot_id.is_empty() || host.boot_id == current_host.boot_id)
}

/// Atomically replace the contents of `path` with `bytes`.
///
/// `pub(super)` so [`super::protocol_v2::manifest_io`] can reuse the
/// exact same write semantics (tempfile + sync + rename + parent
/// fsync) for v2 manifest files.
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ManifestError> {
    atomic_write(path, bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ManifestError> {
    let parent = path
        .parent()
        .ok_or_else(|| ManifestError::MissingParent(path.to_path_buf()))?;
    fs::create_dir_all(parent)?;
    let tmp = temp_path_for(path);

    let write_result = (|| -> Result<(), ManifestError> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&tmp, path)?;
        sync_parent(parent)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

fn temp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("manifest.pb");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.with_file_name(format!(".{file_name}.tmp-{}-{nanos}", std::process::id()))
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, target: &Path) -> io::Result<()> {
    fs::rename(tmp, target)
}

#[cfg(windows)]
fn replace_file(tmp: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{ReplaceFileW, REPLACEFILE_WRITE_THROUGH};

    if !target.exists() {
        return fs::rename(tmp, target);
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let target_w = wide(target);
    let tmp_w = wide(tmp);
    let ok = unsafe {
        ReplaceFileW(
            target_w.as_ptr(),
            tmp_w.as_ptr(),
            std::ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn sync_parent(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::protocol::Operation;

    fn sample_manifest() -> CacheManifest {
        let host = host_identity::current();
        CacheManifest {
            manifest_schema_version: 1,
            media_type: CACHE_MANIFEST_MEDIA_TYPE.to_string(),
            self_sha256: Vec::new(),
            host: Some(host),
            current_operation: Some(Operation {
                kind: 0,
                started_at_unix_ms: 1,
                expected_done_unix_ms: 0,
            }),
            valid_until_unix_ms: 0,
            service_name: "zccache".to_string(),
            service_version: "1.2.3".to_string(),
            broker_envelope_version: "v1".to_string(),
            created_at_unix_ms: 1,
            last_active_unix_ms: 2,
            roots: Vec::new(),
            current_daemon: None,
            cleanup_policy: None,
            broker_instance: "shared".to_string(),
            depends_on: Vec::new(),
            provides: Vec::new(),
            observability: None,
            bundle_id: "bundle".to_string(),
        }
    }

    #[test]
    fn self_hash_roundtrip() {
        let manifest = manifest_with_self_sha256(&sample_manifest()).unwrap();
        assert_eq!(manifest.self_sha256.len(), 32);
        verify_self_sha256(&manifest).unwrap();
    }

    #[test]
    fn central_path_validates_inputs() {
        let dir = Path::new("/tmp/registry");
        assert!(central_manifest_path(dir, "zccache", "1.2.3").is_ok());
        assert!(central_manifest_path(dir, "Zccache", "1.2.3").is_err());
        assert!(central_manifest_path(dir, "zccache", "../../../evil").is_err());
    }

    #[test]
    fn central_registry_permissions_are_private_after_ensure() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = tmp.path().join("registry");
        ensure_central_registry_dir(&registry).unwrap();
        assert!(secure_dir::private_dir_permissions_are_private(&registry).unwrap());
    }
}
