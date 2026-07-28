# `lifecycle` tests

Unit tests for the daemon lifecycle module, relocated here when `lifecycle.rs`
crossed the repository's 1,500-line hard ceiling.

| File | Covers |
|---|---|
| `pid_liveness.rs` | `pid_is_live` against real and reaped PIDs (Unix only) |
| `spawn_image.rs` | daemon image resolution, and which env vars cross the spawn boundary |
| `spawn_lock.rs` | spawn-lock acquisition, contention, and release |

Each file retains the `mod` wrapper it had while inline, so the split is a pure
relocation. The one edit was `use super::*` → `use crate::daemon::lifecycle::*`,
since the parent module is now two levels up.
