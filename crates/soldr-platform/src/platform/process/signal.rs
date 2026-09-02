//! SIGTERM detection for the daemon's fast-exit hook.
//!
//! Issue #1286 (F1) originally added this so container stops and `pkill
//! soldr-daemon` would trigger a graceful drain instead of an abrupt kill.
//! soldr#3059 reversed that: the caller (`soldr-daemon`'s
//! `fast_exit_on_signal`) now treats an arriving SIGTERM as a signal to
//! exit within milliseconds — write an end-of-stream marker, say so on
//! stderr, exit non-zero — rather than to drain. This module only detects
//! that the signal arrived; it carries no opinion about what the caller
//! does in response. Unix hosts register a `tokio` SIGTERM signal stream;
//! Windows has no POSIX SIGTERM and parks the hook task forever.

pub use crate::platform_imp::process::signal::wait_for_terminate_signal;
