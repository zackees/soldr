//! Shared path computation for daemon socket, PID file, database, and shadow directory.
//!
//! Both the server and client modules use these functions to agree on where
//! the daemon listens and where auxiliary files are stored.

use std::path::PathBuf;

/// Returns the local socket name the daemon listens on.
///
/// - **Linux/macOS**: `$XDG_RUNTIME_DIR/running-process/daemon{-hash}.sock`
///   (fallback: `/tmp/running-process-{uid}/daemon{-hash}.sock`)
/// - **Windows**: `\\.\pipe\running-process-daemon-{username}{-hash}`
///
/// On Windows the returned string is a full named pipe path that should be
/// passed to [`interprocess::local_socket::ToNsName::to_ns_name`] with
/// [`GenericNamespaced`](interprocess::local_socket::GenericNamespaced).
/// On Unix it is a filesystem path for
/// [`interprocess::local_socket::ToFsName::to_fs_name`] with
/// [`GenericFilePath`](interprocess::local_socket::GenericFilePath).
pub fn socket_path(scope_hash: Option<&str>) -> String {
    #[cfg(unix)]
    {
        // Ensure the directory exists.
        let _ = std::fs::create_dir_all(runtime_dir_unix());
    }
    socket_path_view(scope_hash)
}

/// Read-only variant of [`socket_path`]: derives the same endpoint string
/// without creating any directory. Used by read-only inspectors (#391).
pub fn socket_path_view(scope_hash: Option<&str>) -> String {
    let suffix = match scope_hash {
        Some(h) => format!("-{h}"),
        None => String::new(),
    };

    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        format!(r"\\.\pipe\running-process-daemon-{username}{suffix}")
    }

    #[cfg(unix)]
    {
        format!("{}/daemon{suffix}.sock", runtime_dir_unix().display())
    }
}

/// Build an `interprocess` local socket [`interprocess::local_socket::Name`]
/// from the path returned by [`socket_path`].
///
/// This must use the same name-type dispatch as the server so that client
/// and server agree on the actual IPC endpoint.
pub fn make_socket_name(path: &str) -> std::io::Result<interprocess::local_socket::Name<'_>> {
    use interprocess::local_socket::prelude::*;

    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        path.to_fs_name::<GenericFilePath>()
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        path.to_ns_name::<GenericNamespaced>()
    }
}

/// Returns the path to the daemon PID file.
///
/// - **Linux/macOS**: same directory as the socket, with `.pid` extension.
/// - **Windows**: `%LOCALAPPDATA%\running-process\daemon{-hash}.pid`
pub fn pid_file_path(scope_hash: Option<&str>) -> PathBuf {
    let path = pid_file_path_view(scope_hash);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    path
}

/// Read-only variant of [`pid_file_path`]: derives the same path without
/// creating any directory. Used by read-only inspectors (#391).
pub fn pid_file_path_view(scope_hash: Option<&str>) -> PathBuf {
    let suffix = match scope_hash {
        Some(h) => format!("-{h}"),
        None => String::new(),
    };

    #[cfg(windows)]
    {
        local_app_data_dir().join(format!("daemon{suffix}.pid"))
    }

    #[cfg(unix)]
    {
        runtime_dir_unix().join(format!("daemon{suffix}.pid"))
    }
}

/// Returns the path to the daemon SQLite database.
///
/// - **Linux/macOS**: `$XDG_STATE_HOME/running-process/tracked-pids{-hash}.sqlite3`
///   (fallback: `~/.local/state/running-process/tracked-pids{-hash}.sqlite3`)
/// - **Windows**: `%LOCALAPPDATA%\running-process\tracked-pids{-hash}.sqlite3`
pub fn db_path(scope_hash: Option<&str>) -> PathBuf {
    let path = db_path_view(scope_hash);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    path
}

/// Read-only variant of [`db_path`]: derives the same path without creating
/// any directory. Used by read-only inspectors (#391).
pub fn db_path_view(scope_hash: Option<&str>) -> PathBuf {
    let suffix = match scope_hash {
        Some(h) => format!("-{h}"),
        None => String::new(),
    };
    data_dir().join(format!("tracked-pids{suffix}.sqlite3"))
}

/// Returns the shadow directory used for ephemeral run data.
///
/// - **Windows**: `%LOCALAPPDATA%\running-process\run\`
/// - **Linux**: `$XDG_RUNTIME_DIR/running-process/run/`
/// - **macOS**: `$HOME/Library/Caches/running-process/run/`
pub fn shadow_dir() -> PathBuf {
    let dir = shadow_dir_view();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Read-only variant of [`shadow_dir`]: derives the same path without
/// creating any directory. Used by read-only inspectors (#391).
pub fn shadow_dir_view() -> PathBuf {
    #[cfg(windows)]
    {
        local_app_data_dir().join("run")
    }

    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        home.join("Library/Caches/running-process/run")
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        runtime_dir_unix().join("run")
    }
}

/// Returns the daemon data directory (where the SQLite tracking database
/// lives) WITHOUT creating it. Read-only callers (doctor, status probes)
/// use this; [`db_path`] keeps its create-on-derive behavior.
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        local_app_data_dir()
    }

    #[cfg(unix)]
    {
        state_dir_unix()
    }
}

// ---------------------------------------------------------------------------
// Platform helpers
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn local_app_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("running-process")
}

#[cfg(unix)]
fn runtime_dir_unix() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(d).join("running-process")
    } else {
        // Fallback: /tmp/running-process-{uid}
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/running-process-{uid}"))
    }
}

#[cfg(unix)]
fn state_dir_unix() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(d).join("running-process")
    } else if let Some(home) = dirs::home_dir() {
        home.join(".local/state/running-process")
    } else {
        PathBuf::from("/tmp/running-process-state")
    }
}
