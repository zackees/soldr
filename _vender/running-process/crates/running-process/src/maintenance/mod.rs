//! Maintenance subcommands exposed via the `runpm` CLI.
//!
//! Phase 1 of #228 (issue #230) lands a single subcommand:
//! [`release_handles`], the cross-platform foundation for the Windows
//! worktree-teardown handle-race fix (soldr#710).

pub mod release_handles;

pub use release_handles::{run_release_handles, ReleaseHandlesError, ReleaseHandlesOutcome};
