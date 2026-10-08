//! Portable transport adapter. The backend owns its snapshot format and locks.

use std::{io, path::Path};

use crate::core::SoldrPaths;
use zccache::artifact::snapshot::{export_store_snapshot, import_snapshot, SnapshotReceipt};

/// Stable logical archive path, independent of the broker's execution route.
pub const ARCHIVE_PREFIX: &str = "zccache/compiler-snapshot-v1";
pub const PRIVATE_PREFIX: &str = "zccache/daemon-state";

fn compatibility() -> String {
    // Store format compatibility is separate from compiler compatibility:
    // the backend still checks each compiler context when replaying an object.
    let identity = format!(
        "soldr-embedded-snapshot-v1:{}",
        zccache::core::config::versioned_subdir()
    );
    zccache::hash::hash_bytes(identity.as_bytes())
        .to_hex()
        .to_string()
}

/// The caller must checkpoint and gracefully stop the selected daemon first.
pub fn export(paths: &SoldrPaths, destination: &Path) -> io::Result<Option<SnapshotReceipt>> {
    let source = crate::zccache_embedded::embedded_version_root(paths);
    if !source.exists() {
        return Ok(None);
    }
    export_store_snapshot(&source, &compatibility(), destination).map(Some)
}

/// Import before daemon startup. Existing destination stores are never replaced.
pub fn import(paths: &SoldrPaths, source: &Path) -> io::Result<SnapshotReceipt> {
    let destination = crate::zccache_embedded::embedded_version_root(paths);
    import_snapshot(source, &compatibility(), &destination)
}
