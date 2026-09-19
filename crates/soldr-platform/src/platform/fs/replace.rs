//! Atomic replacement and open/running-image retirement.

pub use crate::platform_imp::fs::replace::{open_for_retire, retire_open_file};

/// Atomically replace `destination` with `source`.
///
/// Delegates to `kernal_api::platform::fs::replacement::atomic_replace`
/// (soldr#3297): Unix `rename`, Windows `MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`. kernal-api's
/// implementation is a strict superset of soldr's former per-platform
/// copies — same Unix rename, same Windows API and flags, plus long-path
/// support and a retry ladder for transient antivirus/indexer sharing
/// violations on Windows that soldr's own copies did not have. Soldr no
/// longer carries its own `atomic_replace`.
pub use kernal_api::platform::fs::replacement::atomic_replace;

#[cfg(test)]
mod tests {
    use std::fs;

    use super::atomic_replace;

    #[test]
    fn atomic_replace_moves_source_bytes_onto_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        fs::write(&source, b"new contents").expect("write source");
        fs::write(&destination, b"stale contents").expect("write destination");

        atomic_replace(&source, &destination).expect("atomic_replace");

        assert_eq!(
            fs::read(&destination).expect("read destination"),
            b"new contents"
        );
        assert!(!source.exists(), "source must be consumed by the replace");
    }
}
