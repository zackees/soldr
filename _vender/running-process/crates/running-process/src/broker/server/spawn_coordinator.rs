//! Spawn coordination contract for broker-managed backends.
//!
//! This module does not launch child processes yet. It owns the state that
//! Phase 4/5 launch code needs before spawning: per-backend-key budget windows,
//! single-flight protection, retry-after hints for refused Hello replies, and
//! process-wide file locks for backend spawn ownership.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::backend_registry::BackendKey;

/// Default backend spawn attempts allowed per budget window.
pub const DEFAULT_SPAWN_ATTEMPTS_PER_WINDOW: u32 = 3;

/// Default backend spawn budget window.
pub const DEFAULT_SPAWN_BUDGET_WINDOW: Duration = Duration::from_secs(30);

/// Acquire the backend spawn lock at `path`.
///
/// The returned guard owns an exclusive OS file lock until it is dropped. The
/// lock file is intentionally left in place on drop; ownership is attached to
/// the open file handle, not to path existence.
///
/// On Unix and Windows this helper verifies file identity after taking the
/// lock. If another coordinator deletes, renames, or recreates the lock file
/// between open and lock acquisition, the helper refuses the stale handle with
/// [`SpawnLockError::DeletedOrRecreated`]. On platforms where file identity is
/// not available, callers must keep lock files in a trusted broker-owned
/// directory and treat path deletion/recreation detection as best-effort.
pub fn acquire_spawn_lock(path: impl AsRef<Path>) -> Result<SpawnLockGuard, SpawnLockError> {
    acquire_spawn_lock_with_hook(path.as_ref(), |_, _| {})
}

fn acquire_spawn_lock_with_hook<F>(
    path: &Path,
    mut before_lock: F,
) -> Result<SpawnLockGuard, SpawnLockError>
where
    F: FnMut(&Path, &File),
{
    let path_buf = path.to_path_buf();
    let file = open_lock_file(path).map_err(|source| SpawnLockError::Open {
        path: path_buf.clone(),
        source,
    })?;

    before_lock(path, &file);

    try_lock_file(&file).map_err(|source| {
        if is_lock_conflict(&source) {
            SpawnLockError::AlreadyLocked {
                path: path_buf.clone(),
            }
        } else {
            SpawnLockError::Lock {
                path: path_buf.clone(),
                source,
            }
        }
    })?;

    let opened_identity =
        file_identity(&file).map_err(|source| lock_identity_error(&path_buf, &file, source))?;
    let current_identity = match path_identity(path) {
        Ok(identity) => identity,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            let _ = try_unlock_file(&file);
            return Err(SpawnLockError::DeletedOrRecreated {
                path: path_buf,
                opened_identity,
                current_identity: None,
            });
        }
        Err(source) => return Err(lock_identity_error(&path_buf, &file, source)),
    };

    if opened_identity != current_identity {
        let _ = try_unlock_file(&file);
        return Err(SpawnLockError::DeletedOrRecreated {
            path: path_buf,
            opened_identity,
            current_identity,
        });
    }

    Ok(SpawnLockGuard {
        file,
        path: path_buf,
        identity: opened_identity,
    })
}

fn lock_identity_error(path: &Path, file: &File, source: io::Error) -> SpawnLockError {
    let _ = try_unlock_file(file);
    SpawnLockError::Identity {
        path: path.to_path_buf(),
        source,
    }
}

/// RAII guard for an acquired backend spawn lock.
#[must_use = "dropping the guard releases the backend spawn lock immediately"]
#[derive(Debug)]
pub struct SpawnLockGuard {
    file: File,
    path: PathBuf,
    identity: Option<SpawnLockFileIdentity>,
}

impl SpawnLockGuard {
    /// Lock file path that was acquired.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Platform file identity captured for the lock file, when available.
    pub fn file_identity(&self) -> Option<SpawnLockFileIdentity> {
        self.identity
    }
}

impl Drop for SpawnLockGuard {
    fn drop(&mut self) {
        let _ = try_unlock_file(&self.file);
    }
}

/// Stable identity for an opened lock file on platforms that expose it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnLockFileIdentity {
    /// Device, volume, or platform-equivalent file namespace.
    pub device: u64,
    /// Inode, file index, or platform-equivalent file number.
    pub file: u64,
}

/// Errors returned while acquiring a backend spawn lock.
#[derive(Debug, thiserror::Error)]
pub enum SpawnLockError {
    /// The lock path could not be opened or created.
    #[error("failed to open backend spawn lock file {path}: {source}")]
    Open {
        /// Lock path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// Another broker worker already owns the lock.
    #[error("backend spawn lock file {path} is already locked")]
    AlreadyLocked {
        /// Lock path.
        path: PathBuf,
    },
    /// The platform lock operation failed for a reason other than contention.
    #[error("failed to lock backend spawn lock file {path}: {source}")]
    Lock {
        /// Lock path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The lock path no longer names the file that was locked.
    #[error("backend spawn lock file {path} was deleted or recreated during acquisition")]
    DeletedOrRecreated {
        /// Lock path.
        path: PathBuf,
        /// Identity of the opened file handle.
        opened_identity: Option<SpawnLockFileIdentity>,
        /// Identity currently reachable through the lock path.
        current_identity: Option<SpawnLockFileIdentity>,
    },
    /// File identity could not be read.
    #[error("failed to verify backend spawn lock file identity for {path}: {source}")]
    Identity {
        /// Lock path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
}

/// Spawn-budget tuning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnBudgetConfig {
    /// Maximum spawn attempts in one window.
    pub max_attempts: u32,
    /// Window duration.
    pub window: Duration,
}

impl SpawnBudgetConfig {
    /// Build a config, clamping zero values to safe non-zero defaults.
    pub fn new(max_attempts: u32, window: Duration) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            window: if window.is_zero() {
                Duration::from_millis(1)
            } else {
                window
            },
        }
    }
}

impl Default for SpawnBudgetConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_SPAWN_ATTEMPTS_PER_WINDOW,
            window: DEFAULT_SPAWN_BUDGET_WINDOW,
        }
    }
}

/// Coordinates bounded spawn attempts for backend keys.
#[derive(Debug)]
pub struct SpawnCoordinator {
    config: SpawnBudgetConfig,
    states: HashMap<BackendKey, SpawnBudgetState>,
}

impl SpawnCoordinator {
    /// Create an empty coordinator with default budget settings.
    pub fn new() -> Self {
        Self::with_config(SpawnBudgetConfig::default())
    }

    /// Create an empty coordinator with explicit budget settings.
    pub fn with_config(config: SpawnBudgetConfig) -> Self {
        Self {
            config,
            states: HashMap::new(),
        }
    }

    /// Begin one spawn attempt for `key`.
    ///
    /// The returned permit is a contract token for the caller that will perform
    /// the actual child-process launch in later slices. Call [`Self::finish`]
    /// when that launch path succeeds or fails.
    pub fn try_begin(
        &mut self,
        key: BackendKey,
        now: Instant,
    ) -> Result<SpawnPermit, SpawnBeginError> {
        let state = self
            .states
            .entry(key.clone())
            .or_insert_with(|| SpawnBudgetState::new(now));
        state.refresh(now, self.config.window);

        if state.in_flight {
            return Err(SpawnBeginError::AlreadyInProgress);
        }

        if state.attempts_used >= self.config.max_attempts {
            return Err(SpawnBeginError::BudgetExhausted {
                retry_after: retry_after(state.window_started_at, now, self.config.window),
                remaining: 0,
            });
        }

        state.attempts_used += 1;
        state.in_flight = true;
        Ok(SpawnPermit {
            key,
            attempt_number: state.attempts_used,
            remaining_after_begin: self.config.max_attempts - state.attempts_used,
        })
    }

    /// Finish an in-flight spawn attempt.
    pub fn finish(&mut self, key: &BackendKey, outcome: SpawnOutcome, now: Instant) {
        let Some(state) = self.states.get_mut(key) else {
            return;
        };
        state.refresh(now, self.config.window);
        state.in_flight = false;
        if outcome == SpawnOutcome::Success {
            state.window_started_at = now;
            state.attempts_used = 0;
        }
    }

    /// Return the current budget snapshot for one backend key.
    pub fn snapshot(&mut self, key: BackendKey, now: Instant) -> SpawnBudgetSnapshot {
        let state = self
            .states
            .entry(key.clone())
            .or_insert_with(|| SpawnBudgetState::new(now));
        state.refresh(now, self.config.window);
        snapshot_for(key, state, self.config, now)
    }
}

impl Default for SpawnCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// Token returned for a spawn attempt that may proceed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnPermit {
    /// Backend key this permit covers.
    pub key: BackendKey,
    /// 1-based attempt number inside the current window.
    pub attempt_number: u32,
    /// Budget remaining after this attempt starts.
    pub remaining_after_begin: u32,
}

/// Result of a spawn attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// The backend process was launched and verified.
    Success,
    /// The backend process failed to launch or verify.
    Failed,
}

/// Errors returned when a spawn attempt cannot begin.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SpawnBeginError {
    /// Another worker is already launching this backend key.
    #[error("backend spawn already in progress")]
    AlreadyInProgress,
    /// The per-key spawn budget is exhausted.
    #[error("backend spawn budget exhausted; retry after {retry_after:?}")]
    BudgetExhausted {
        /// Time until the budget window resets.
        retry_after: Duration,
        /// Remaining attempts, always zero for this variant.
        remaining: u32,
    },
}

/// Current budget state for metrics/admin snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnBudgetSnapshot {
    /// Backend key this snapshot describes.
    pub key: BackendKey,
    /// Attempts used in the active window.
    pub attempts_used: u32,
    /// Attempts still available in the active window.
    pub remaining: u32,
    /// Whether a spawn is currently in flight.
    pub in_flight: bool,
    /// Retry-after hint when no attempts remain.
    pub retry_after: Option<Duration>,
}

#[derive(Clone, Debug)]
struct SpawnBudgetState {
    window_started_at: Instant,
    attempts_used: u32,
    in_flight: bool,
}

impl SpawnBudgetState {
    fn new(now: Instant) -> Self {
        Self {
            window_started_at: now,
            attempts_used: 0,
            in_flight: false,
        }
    }

    fn refresh(&mut self, now: Instant, window: Duration) {
        if elapsed_since(self.window_started_at, now) >= window {
            self.window_started_at = now;
            self.attempts_used = 0;
            self.in_flight = false;
        }
    }
}

fn snapshot_for(
    key: BackendKey,
    state: &SpawnBudgetState,
    config: SpawnBudgetConfig,
    now: Instant,
) -> SpawnBudgetSnapshot {
    let remaining = config.max_attempts.saturating_sub(state.attempts_used);
    SpawnBudgetSnapshot {
        key,
        attempts_used: state.attempts_used,
        remaining,
        in_flight: state.in_flight,
        retry_after: (remaining == 0)
            .then(|| retry_after(state.window_started_at, now, config.window)),
    }
}

fn retry_after(window_started_at: Instant, now: Instant, window: Duration) -> Duration {
    window.saturating_sub(elapsed_since(window_started_at, now))
}

fn elapsed_since(started_at: Instant, now: Instant) -> Duration {
    now.checked_duration_since(started_at)
        .unwrap_or(Duration::ZERO)
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    configure_lock_file_options(&mut options);
    options.open(path)
}

#[cfg(unix)]
fn configure_lock_file_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(windows)]
fn configure_lock_file_options(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    use winapi::um::winnt::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

    options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
}

#[cfg(not(any(unix, windows)))]
fn configure_lock_file_options(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn try_lock_file(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn try_unlock_file(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn is_lock_conflict(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::EWOULDBLOCK) || error.raw_os_error() == Some(libc::EAGAIN)
}

#[cfg(unix)]
fn file_identity(file: &File) -> io::Result<Option<SpawnLockFileIdentity>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok(Some(SpawnLockFileIdentity {
        device: metadata.dev(),
        file: metadata.ino(),
    }))
}

#[cfg(unix)]
fn path_identity(path: &Path) -> io::Result<Option<SpawnLockFileIdentity>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = path.metadata()?;
    Ok(Some(SpawnLockFileIdentity {
        device: metadata.dev(),
        file: metadata.ino(),
    }))
}

#[cfg(windows)]
fn try_lock_file(file: &File) -> io::Result<()> {
    use std::mem;
    use std::os::windows::io::AsRawHandle;
    use winapi::um::fileapi::LockFileEx;
    use winapi::um::minwinbase::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, OVERLAPPED};
    use winapi::um::winnt::HANDLE;

    let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn try_unlock_file(file: &File) -> io::Result<()> {
    use std::mem;
    use std::os::windows::io::AsRawHandle;
    use winapi::um::fileapi::UnlockFileEx;
    use winapi::um::minwinbase::OVERLAPPED;
    use winapi::um::winnt::HANDLE;

    let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
    let result = unsafe {
        UnlockFileEx(
            file.as_raw_handle() as HANDLE,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn is_lock_conflict(error: &io::Error) -> bool {
    use winapi::shared::winerror::ERROR_LOCK_VIOLATION;

    error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32)
}

#[cfg(windows)]
fn file_identity(file: &File) -> io::Result<Option<SpawnLockFileIdentity>> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use winapi::um::fileapi::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    use winapi::um::winnt::HANDLE;

    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, info.as_mut_ptr()) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    let info = unsafe { info.assume_init() };
    Ok(Some(SpawnLockFileIdentity {
        device: info.dwVolumeSerialNumber as u64,
        file: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    }))
}

#[cfg(windows)]
fn path_identity(path: &Path) -> io::Result<Option<SpawnLockFileIdentity>> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    configure_lock_file_options(&mut options);
    let file = options.open(path)?;
    file_identity(&file)
}

#[cfg(not(any(unix, windows)))]
fn try_lock_file(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "backend spawn file locks are supported only on Unix and Windows",
    ))
}

#[cfg(not(any(unix, windows)))]
fn try_unlock_file(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn is_lock_conflict(_error: &io::Error) -> bool {
    false
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &File) -> io::Result<Option<SpawnLockFileIdentity>> {
    Ok(None)
}

#[cfg(not(any(unix, windows)))]
fn path_identity(_path: &Path) -> io::Result<Option<SpawnLockFileIdentity>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    #[cfg(any(unix, windows))]
    fn acquire_spawn_lock_detects_lock_file_replacement_between_open_and_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("backend.spawn.lock");
        let replaced_path = tmp.path().join("backend.spawn.lock.replaced");

        let err = acquire_spawn_lock_with_hook(&lock_path, |path, _file| {
            fs::rename(path, &replaced_path).unwrap();
            fs::write(path, b"replacement lock file").unwrap();
        })
        .unwrap_err();

        let SpawnLockError::DeletedOrRecreated {
            path,
            opened_identity: Some(opened_identity),
            current_identity: Some(current_identity),
        } = err
        else {
            panic!("expected deleted/recreated error, got {err:?}");
        };

        assert_eq!(path, lock_path);
        assert_ne!(opened_identity, current_identity);

        let _guard = acquire_spawn_lock(&lock_path).unwrap();
    }
}
