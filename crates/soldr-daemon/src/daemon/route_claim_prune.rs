//! soldr#3685: route-claim reads that remember the exact bytes observed, and
//! a prune that removes only those bytes (compare-and-delete).

use std::io;

use super::backend_handle_adoption::broker_route_claim_path;
use crate::core::SoldrPaths;
use running_process::broker::backend_handle::DaemonProcess;

/// One read of the route claim: the exact bytes observed plus their decode.
///
/// soldr#3685: callers that prune must prune *these* bytes, never "whatever
/// is at the path now" -- a daemon may have atomically published a fresh
/// claim between the read and the prune.
pub struct RouteClaimSnapshot {
    pub bytes: Vec<u8>,
    /// `Err` is always `InvalidData`: the bytes were read but are not a claim.
    pub claim: io::Result<DaemonProcess>,
}

/// Read the route claim. An IO error (including a Windows sharing violation
/// during a concurrent `atomic_replace`) is returned as `Err` and is
/// inconclusive: it says nothing about whether the claim is corrupt.
pub fn read_broker_route_claim_snapshot(
    paths: &SoldrPaths,
) -> io::Result<Option<RouteClaimSnapshot>> {
    let bytes = match std::fs::read(broker_route_claim_path(paths)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let claim = decode_broker_route_claim(&bytes);
    Ok(Some(RouteClaimSnapshot { bytes, claim }))
}

fn decode_broker_route_claim(bytes: &[u8]) -> io::Result<DaemonProcess> {
    use prost::Message as _;
    let claim = running_process::broker::protocol::DaemonProcess::decode(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    DaemonProcess::try_from(claim)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Remove the route claim only if it still holds `expected` (soldr#3685).
///
/// The claim is first renamed to a unique tombstone, which atomically takes
/// exactly one version of it. If that version is not `expected`, a newer
/// claim was published after the caller's read: it is restored with a
/// no-clobber hard link (a still newer claim at the path wins) and kept.
/// Returns whether the expected claim was removed.
pub fn prune_broker_route_claim_if_unchanged(
    paths: &SoldrPaths,
    expected: &[u8],
) -> io::Result<bool> {
    let claim_path = broker_route_claim_path(paths);
    let Some(directory) = claim_path.parent() else {
        return Ok(false);
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tombstone = directory.join(format!(
        ".route-claim.tombstone.{}.{nanos}",
        std::process::id()
    ));
    match std::fs::rename(&claim_path, &tombstone) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let taken = std::fs::read(&tombstone);
    if matches!(&taken, Ok(bytes) if bytes.as_slice() == expected) {
        std::fs::remove_file(&tombstone)?;
        return Ok(true);
    }
    match std::fs::hard_link(&tombstone, &claim_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => {
            // Could not restore by link; fall back to a rename so the newer
            // claim is never lost.
            if !claim_path.exists() {
                std::fs::rename(&tombstone, &claim_path)?;
                return Ok(false);
            }
        }
    }
    std::fs::remove_file(&tombstone)?;
    taken.map(|_| false)
}

/// Read the claim for adoption: `Some((claim, bytes))` when it decodes.
/// Corrupt bytes are pruned (compare-and-delete) when `prune_invalid`; an IO
/// error is inconclusive and leaves the claim in place.
pub fn read_route_claim_pruning_corrupt(
    paths: &SoldrPaths,
    prune_invalid: bool,
) -> Option<(DaemonProcess, Vec<u8>)> {
    let path = broker_route_claim_path(paths);
    let snapshot = match read_broker_route_claim_snapshot(paths) {
        Ok(snapshot) => snapshot?,
        Err(error) => {
            eprintln!(
                "soldr broker: daemon route claim {} unreadable, kept: {error}",
                path.display()
            );
            return None;
        }
    };
    match snapshot.claim {
        Ok(claim) => Some((claim, snapshot.bytes)),
        Err(error) => {
            eprintln!(
                "soldr broker: pruning corrupt daemon route claim {}: {error}",
                path.display()
            );
            if prune_invalid {
                let _ = prune_broker_route_claim_if_unchanged(paths, &snapshot.bytes);
            }
            None
        }
    }
}

#[cfg(test)]
#[path = "route_claim_prune_tests.rs"]
mod tests;
