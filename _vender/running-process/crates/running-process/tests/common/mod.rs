//! Shared helpers for the MITM PTY substrate integration tests
//! (#448, #449).
//!
//! Lives under `tests/common/` per the conventional integration-test
//! pattern: each `tests/*.rs` file is its own crate; declare
//! `mod common;` to pull this module in. Helpers here build the
//! testbin on demand, spawn a `NativePtyProcess` around it, and
//! expose byte-level write/drain primitives so each test can focus
//! on the scenario it asserts on.

#![allow(dead_code)] // each test crate uses a subset of helpers

pub mod mitm_stdin;
