//! Request handlers for the daemon's IPC protocol.
//!
//! Each handler receives a [`crate::proto::daemon::DaemonRequest`] and a shared
//! [`DaemonState`] reference, returning a fully-constructed
//! [`crate::proto::daemon::DaemonResponse`].

use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;

use crate::daemon::pipe_sessions::PipeSessionRegistry;
use crate::daemon::pty_sessions::PtySessionRegistry;
use crate::daemon::registry::Registry;

// Re-import proto types used by the test module so `use super::*;` picks
// them up. Tests also rely on `Arc`, `Instant`, `AtomicU32`, `watch`, and
// `Registry` being in scope via the same glob — those are imported above.
#[cfg(test)]
#[allow(unused_imports)]
use crate::proto::daemon::{DaemonRequest, ProcessState, StatusCode};

mod core;
mod kill;
mod maintenance;
mod observer;
mod pipe_sessions_handlers;
mod process_tree;
mod pty_sessions_handlers;
mod registry_handlers;
mod services;
mod spawn;
mod telemetry;
mod util;

pub use self::core::{handle_ping, handle_shutdown, handle_status};
pub use self::kill::{handle_kill_tree, handle_kill_zombies};
pub use self::maintenance::{
    handle_bulk_terminate_sessions, handle_get_session_backlog, handle_purge_exited_sessions,
};
pub use self::observer::{
    handle_get_session_observer_status, handle_register_session_observer,
    handle_unregister_session_observer,
};
pub use self::pipe_sessions_handlers::{
    handle_attach_pipe_stream, handle_detach_pipe_stream, handle_list_pipe_sessions,
    handle_spawn_pipe_session, handle_terminate_pipe_session, handle_write_pipe_stdin,
};
pub use self::process_tree::handle_get_process_tree;
pub use self::pty_sessions_handlers::{
    handle_attach_pty_session, handle_detach_pty_session, handle_list_pty_sessions,
    handle_resize_pty_session, handle_spawn_pty_session, handle_terminate_pty_session,
};
pub use self::registry_handlers::{
    handle_list_active, handle_list_by_originator, handle_register, handle_unregister,
};
pub use self::services::{
    handle_service_delete, handle_service_describe, handle_service_flush, handle_service_list,
    handle_service_logs, handle_service_restart, handle_service_resurrect, handle_service_save,
    handle_service_start, handle_service_stop,
};
pub use self::spawn::handle_spawn_daemon;
pub use self::telemetry::{
    handle_get_session_tee_status, handle_register_session_tee, handle_unregister_session_tee,
};

// ---------------------------------------------------------------------------
// Shared daemon state
// ---------------------------------------------------------------------------

/// Shared state accessible by all request handlers.
///
/// Created once when the server starts and wrapped in an `Arc` so that every
/// connection handler can read (and, for atomics, update) it concurrently.
pub struct DaemonState {
    /// When the daemon process started.
    pub start_time: Instant,
    /// Crate / workspace version string.
    pub version: String,
    /// The IPC socket path the daemon is listening on.
    pub socket_path: String,
    /// Path to the SQLite tracking database.
    pub db_path: String,
    /// Human-readable scope name (e.g. project directory).
    pub scope: String,
    /// FNV-1a hash of the scope (used in file/pipe names).
    pub scope_hash: String,
    /// Working directory that produced the scope hash.
    pub scope_cwd: String,
    /// Channel used to signal the server to shut down.
    pub shutdown_tx: watch::Sender<bool>,
    /// Number of currently active client connections.
    pub active_connections: AtomicU32,
    /// SQLite-backed process registry.
    pub registry: Arc<Registry>,
    /// In-memory registry of daemon-owned PTY sessions (issue #130 M2).
    pub pty_sessions: Arc<PtySessionRegistry>,
    /// In-memory registry of daemon-owned pipe sessions (issue #130 M3).
    pub pipe_sessions: Arc<PipeSessionRegistry>,
    /// SQLite-backed registry of supervised services (runpm Phase 2, #222).
    pub services: Arc<crate::daemon::services::ServiceRegistry>,
    /// Pre-allocated ENOSPC delete-to-recover reserve (#390).
    pub emergency_reserve: Arc<crate::daemon::emergency_reserve::EmergencyReserve>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "../handlers_tests.rs"]
mod tests;
