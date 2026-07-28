# `lifecycle` tests

Unit tests for the daemon lifecycle module, relocated here when `lifecycle.rs`
crossed the repository's 1,500-line hard ceiling.

| File | Covers |
|---|---|
| `pid_liveness.rs` | `pid_is_live` against real and reaped PIDs (Unix only) |
| `spawn_image.rs` | daemon image resolution, and which env vars cross the spawn boundary |
| `spawn_lock.rs` | spawn-lock acquisition, contention, release, and stale-daemon displacement |

Each file retains the `mod` wrapper it had while inline, so the move is a pure
relocation with no reindentation. The one edit was `use super::*` →
`use crate::daemon::lifecycle::*`, since the parent module is now two levels up.
