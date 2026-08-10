//! ENOSPC delete-to-recover emergency reserve (#390).
//!
//! At startup the daemon pre-allocates a 32 MiB sacrificial file next to
//! the SQLite tracking database. When a daemon write path later fails
//! with ENOSPC (disk full), the reserve file is deleted to regain just
//! enough headroom for clean shutdown bookkeeping and logging. The
//! lifecycle is idempotent: the file is recreated on the next startup, a
//! missing file is tolerated, and a startup that itself hits ENOSPC logs
//! the failure and continues with a degraded flag instead of failing.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing::{error, info, warn};

/// Size of the pre-allocated emergency file.
pub const EMERGENCY_RESERVE_BYTES: u64 = 32 * 1024 * 1024;
/// Leaf file name, placed alongside the SQLite database.
pub const EMERGENCY_RESERVE_FILE_NAME: &str = "emergency-reserve.bin";

/// Lifecycle state of the emergency reserve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReserveState {
    /// Reserve file exists at its full size and can be released.
    Armed,
    /// Reserve was deleted in response to an ENOSPC event.
    Released,
    /// Pre-allocation failed (often itself ENOSPC); daemon runs without
    /// the safety margin.
    Degraded,
}

impl ReserveState {
    /// Stable lowercase label used in logs and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            ReserveState::Armed => "armed",
            ReserveState::Released => "released",
            ReserveState::Degraded => "degraded",
        }
    }
}

/// Pre-allocated disk reserve with delete-to-recover semantics.
#[derive(Debug)]
pub struct EmergencyReserve {
    path: PathBuf,
    size: u64,
    state: Mutex<ReserveState>,
}

impl EmergencyReserve {
    /// Create (or recreate) the reserve file in `dir` at the default size.
    /// Never fails: pre-allocation errors degrade instead.
    pub fn initialize_in(dir: &Path) -> Self {
        Self::initialize_at(
            dir.join(EMERGENCY_RESERVE_FILE_NAME),
            EMERGENCY_RESERVE_BYTES,
        )
    }

    /// Create (or recreate) the reserve file at `path` with `size` bytes.
    pub fn initialize_at(path: PathBuf, size: u64) -> Self {
        let state = match preallocate(&path, size) {
            Ok(()) => {
                info!(
                    "emergency reserve armed: {} ({} bytes)",
                    path.display(),
                    size
                );
                ReserveState::Armed
            }
            Err(err) => {
                warn!(
                    "emergency reserve pre-allocation failed at {} ({err}); \
                     continuing degraded without ENOSPC headroom",
                    path.display()
                );
                ReserveState::Degraded
            }
        };
        Self {
            path,
            size,
            state: Mutex::new(state),
        }
    }

    /// Reserve file location.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Configured reserve size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Current lifecycle state.
    pub fn state(&self) -> ReserveState {
        *self.lock()
    }

    /// Delete the reserve file to regain disk headroom. Idempotent:
    /// repeated calls and an already-missing file both succeed. Returns
    /// `true` when this call transitioned the reserve to released.
    pub fn release(&self, reason: &str) -> bool {
        let mut state = self.lock();
        if *state == ReserveState::Released {
            return false;
        }
        let was_armed = *state == ReserveState::Armed;
        match std::fs::remove_file(&self.path) {
            Ok(()) => {
                error!(
                    "ENOSPC recovery: released emergency reserve {} ({} bytes) — {reason}",
                    self.path.display(),
                    self.size
                );
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                warn!(
                    "ENOSPC recovery: emergency reserve {} already missing — {reason}",
                    self.path.display()
                );
            }
            Err(err) => {
                error!(
                    "ENOSPC recovery: failed to delete emergency reserve {} ({err}) — {reason}",
                    self.path.display()
                );
                return false;
            }
        }
        *state = ReserveState::Released;
        was_armed
    }

    /// Release the reserve when `err` signals a full disk. Returns `true`
    /// when a release happened in response to this error.
    pub fn release_if_enospc(&self, err: &io::Error, context: &str) -> bool {
        if !is_disk_full_error(err) {
            return false;
        }
        self.release(context)
    }

    /// Heuristic hook for error chains that only surface as strings
    /// (e.g. SQLite's "database or disk is full"). Returns `true` when a
    /// release happened in response to this message.
    pub fn release_if_disk_full_message(&self, message: &str, context: &str) -> bool {
        if !message_signals_disk_full(message) {
            return false;
        }
        self.release(context)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ReserveState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// True when `err` signals an out-of-space condition.
pub fn is_disk_full_error(err: &io::Error) -> bool {
    if matches!(err.kind(), io::ErrorKind::StorageFull) {
        return true;
    }
    let Some(code) = err.raw_os_error() else {
        return false;
    };
    #[cfg(unix)]
    {
        code == libc::ENOSPC || code == libc::EDQUOT
    }
    #[cfg(windows)]
    {
        // ERROR_HANDLE_DISK_FULL, ERROR_DISK_FULL.
        code == 39 || code == 112
    }
}

/// True when an error string mentions a full disk (SQLite / wrapped errors).
pub fn message_signals_disk_full(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("no space left on device")
        || lower.contains("disk is full")
        || lower.contains("disk full")
        || lower.contains("os error 28")
}

/// One platform-appropriate disk-full error (test helper).
pub fn disk_full_error_for_tests() -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(libc::ENOSPC)
    }
    #[cfg(windows)]
    {
        io::Error::from_raw_os_error(112)
    }
}

fn preallocate(path: &Path, size: u64) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Recreate from scratch so a partial file from a crashed run is
    // replaced with a full-size reserve.
    let mut file = std::fs::File::create(path)?;
    file.set_len(size)?;
    // `set_len` produces a sparse file on most filesystems, which would
    // reserve no real blocks. Write through the file so the space is
    // actually committed; chunked to bound memory.
    const CHUNK: usize = 1024 * 1024;
    let chunk = vec![0u8; CHUNK];
    let mut remaining = size;
    while remaining > 0 {
        let take = remaining.min(CHUNK as u64) as usize;
        if let Err(err) = file.write_all(&chunk[..take]) {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(err);
        }
        remaining -= take as u64;
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_reserve_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rp-emergency-reserve-{label}-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn initialize_creates_file_at_configured_size() {
        let path = temp_reserve_path("create");
        let reserve = EmergencyReserve::initialize_at(path.clone(), 64 * 1024);
        assert_eq!(reserve.state(), ReserveState::Armed);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 * 1024);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn release_on_simulated_enospc_deletes_file() {
        let path = temp_reserve_path("release");
        let reserve = EmergencyReserve::initialize_at(path.clone(), 16 * 1024);
        assert!(reserve.release_if_enospc(&disk_full_error_for_tests(), "test write"));
        assert_eq!(reserve.state(), ReserveState::Released);
        assert!(!path.exists());
        // Idempotent: a second ENOSPC does not re-release.
        assert!(!reserve.release_if_enospc(&disk_full_error_for_tests(), "test write"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unrelated_errors_keep_reserve_armed() {
        let path = temp_reserve_path("unrelated");
        let reserve = EmergencyReserve::initialize_at(path.clone(), 16 * 1024);
        let err = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        assert!(!reserve.release_if_enospc(&err, "test write"));
        assert_eq!(reserve.state(), ReserveState::Armed);
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reserve_is_recreated_on_next_startup() {
        let path = temp_reserve_path("recreate");
        let first = EmergencyReserve::initialize_at(path.clone(), 16 * 1024);
        first.release("simulated enospc");
        assert!(!path.exists());

        let second = EmergencyReserve::initialize_at(path.clone(), 16 * 1024);
        assert_eq!(second.state(), ReserveState::Armed);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 16 * 1024);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_tolerated_on_release() {
        let path = temp_reserve_path("missing");
        let reserve = EmergencyReserve::initialize_at(path.clone(), 16 * 1024);
        std::fs::remove_file(&path).unwrap();
        assert!(reserve.release("simulated enospc"));
        assert_eq!(reserve.state(), ReserveState::Released);
    }

    #[test]
    fn unwritable_dir_degrades_instead_of_failing() {
        let path = std::env::temp_dir()
            .join(format!(
                "rp-emergency-reserve-degraded-{}",
                std::process::id()
            ))
            .join("definitely")
            .join("missing-as-a-file.bin");
        // Make pre-allocation fail by occupying the parent as a file.
        let parent = path.parent().unwrap().parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(parent.parent().unwrap()).unwrap();
        std::fs::write(&parent, b"not a directory").unwrap();
        let reserve = EmergencyReserve::initialize_at(path, 16 * 1024);
        assert_eq!(reserve.state(), ReserveState::Degraded);
        let _ = std::fs::remove_file(&parent);
    }

    #[test]
    fn disk_full_message_heuristic_matches_sqlite_and_os_phrasings() {
        assert!(message_signals_disk_full("database or disk is full"));
        assert!(message_signals_disk_full(
            "No space left on device (os error 28)"
        ));
        assert!(!message_signals_disk_full("permission denied"));
    }
}
