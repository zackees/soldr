//! Synchronous IPC client for the running-process daemon.
//!
//! Wave 4 of #165: absorbed from the former `running-process-client`
//! crate. Re-exports preserved at the top level so downstream code that
//! previously imported from `running_process_client::*` keeps working
//! when it switches to `running_process::client::*`.

#[allow(clippy::module_inception)]
pub mod client;
pub(crate) mod deadline_io;
pub mod observer;
pub mod paths;
pub mod pipe_session;
pub mod pty_session;
pub mod telemetry;

pub use client::{
    connect_or_start, daemonize_command, launch_detached, ClientError, DaemonClient,
    SpawnCommandRequest, SpawnedDaemon,
};
pub use observer::{
    RemoteObserverSubscription, SessionObserverBackpressure, SessionObserverKind,
    SessionObserverRequest, SessionObserverStatus,
};
pub use pipe_session::{PipeSpawnRequest, PipeStreamAttachment, SpawnedPipeSession};
pub use pty_session::{AttachError, PtyAttachment, PtySpawnRequest, SpawnedPtySession};
pub use telemetry::{
    SessionTeeBackpressure, SessionTeeFileMode, SessionTeeFileRequest, SessionTeeKind,
    SessionTeeStatus, SessionTeeStream,
};
