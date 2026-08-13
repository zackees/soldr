//! Graceful-termination signal handling.
//!
//! The daemon needs a SIGTERM hook so container stops and `pkill
//! soldr-daemon` trigger the graceful-drain path instead of an abrupt
//! kill (issue #1286 F1). Unix hosts register a `tokio` SIGTERM signal
//! stream; Windows has no POSIX SIGTERM and parks the hook task.

pub use crate::platform_imp::process::signal::wait_for_terminate_signal;
