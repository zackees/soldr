//! Tests for [`crate::daemon::lifecycle`], split out of the former
//! `lifecycle.rs` when it crossed the repository's per-file LOC ceiling.
//!
//! Each file keeps the `mod` wrapper and body it had inline, so the split is a
//! pure relocation: the only edit was repointing `use super::*` at the parent
//! module, which is now two levels up rather than one.

mod events;
mod pid_liveness;
mod root_acquire;
mod spawn_log;
