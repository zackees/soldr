# `lifecycle` tests

Unit tests for the daemon lifecycle module, relocated here when `lifecycle.rs`
crossed the repository's 1,500-line hard ceiling.

| File | Covers |
|---|---|
| `events.rs` | lifecycle event serialization and attribution |
| `journal_hygiene.rs` | lifecycle journal rotation and recovery |
| `pid_liveness.rs` | `pid_is_live` against real and reaped PIDs (Unix only) |
| `root_acquire.rs` | daemon-root ownership and contention |

Each file retains the `mod` wrapper it had while inline, so the move is a pure
relocation with no reindentation. The one edit was `use super::*` →
`use crate::daemon::lifecycle::*`, since the parent module is now two levels up.
