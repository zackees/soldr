//! Integration tests for `crate::maintenance`. Phase 1 of #228 (#230).
//!
//! Gated behind `feature = "client"` because the maintenance module
//! itself is. With `--no-default-features` this crate compiles to an
//! empty integration test binary.

#![cfg(feature = "client")]

#[cfg(unix)]
mod release_handles_unix;
#[cfg(windows)]
mod release_handles_windows;
