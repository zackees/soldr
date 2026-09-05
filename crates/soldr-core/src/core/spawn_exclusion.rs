//! Process-wide exclusion between staged-file writes and child spawns
//! (soldr#3098). The lock itself lives in the `soldr-platform` dependency
//! leaf so the platform spawn primitives can take it; this module is the
//! `crate::core` spelling every crate above the leaf uses.
//!
//! - [`spawn_shared`]: hold across a `Command::spawn()` (or any other
//!   fork/exec) call, and only across it.
//! - [`write_exclusive`]: hold while a file that may later be `execve`d is
//!   open for writing (`load_extract::extract_one`'s staged sibling).
//!
//! See `soldr_platform::process::spawn_exclusion` for the full mechanism.

pub use crate::platform::process::spawn_exclusion::{spawn_shared, write_exclusive};
