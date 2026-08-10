//! Shared backend lifecycle primitives used by broker and direct clients.

pub mod identity;
pub mod probe;
#[cfg(feature = "client-async")]
pub mod probe_async;
pub mod verify_pid;

pub use identity::DaemonProcess;
