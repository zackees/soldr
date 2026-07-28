# `lifecycle`

Daemon process lifecycle: spawning a detached daemon, resolving which binary
image to spawn, the spawn lock that keeps two racing wrappers from starting two
daemons, PID liveness checks, and displacement of a stale-version daemon.

`mod.rs` was formerly `lifecycle.rs`. It was converted to a directory when it
crossed the repository's 1,500-line hard ceiling.

| File | Holds |
|---|---|
| `mod.rs` | spawn paths, image resolution, spawn lock, PID liveness, displacement |
| `spawn_env.rs` | what environment a spawned daemon inherits |
| `tests/` | the three former inline test modules, one file each |

## Spawn environment

`spawn_env.rs` exists because all three spawn paths — `spawn_detached_inner`,
`spawn_detached_self_inner`, and the Windows `merged_windows_environment_block`
— compose the child environment identically, and that composition is a distinct
concern from the spawning itself.

The baseline deliberately is *not* the caller's environment: on Windows it comes
from `CreateEnvironmentBlock`, on Unix it is rebuilt from the passwd entry. A
variable therefore reaches the daemon only if the allowlist admits it — the
whole `SOLDR_*` namespace, plus a short list of individually justified
`ZCCACHE_*` names.

That list is not "zccache variables are forwarded". `ZCCACHE_DISABLE` is
deliberately dropped. A name earns a place only when the daemon's own process is
what reads it, which is what makes scrubbing it a silent misconfiguration rather
than protection — see soldr#1931, where a resolver tier could never fire because
its variable never crossed. `tests/spawn_image.rs` asserts both directions.
