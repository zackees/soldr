//! Host-neutral process facade.
//!
//! Owns command configuration and console suppression, detached/background
//! spawn and process-group setup, process-tree termination and escalation,
//! PID liveness/zombie state and running-image lookup, exit/signal
//! interpretation, and Unix exec replacement versus Windows spawn-and-wait.
//! Callers retain timeout/retry policy, which program to launch, lifecycle
//! state, and diagnostic wording.
//!
//! Filled by the #2493 migration; the concrete implementations live in the
//! `platform_win` / `platform_linux` / `platform_macos` trees and are
//! re-exported here through [`crate::platform_imp`].
