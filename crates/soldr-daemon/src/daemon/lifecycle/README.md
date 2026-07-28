# `lifecycle`

Daemon process lifecycle: spawning a detached daemon, resolving which binary
image to spawn, the spawn lock that keeps two racing wrappers from starting two
daemons, PID liveness checks, and displacement of a stale-version daemon.

`mod.rs` was formerly `lifecycle.rs`. It was converted to a directory when it
crossed the repository's 1,500-line hard ceiling; the tests moved to `tests/`
and nothing else changed.

## Spawn environment

The spawn paths (`spawn_detached_inner`, `spawn_detached_self_inner`, and the
Windows `merged_windows_environment_block`) all compose the child environment
the same way: `running-process`'s scrubbed user-baseline environment, with a
narrow allowlist overlaid on top.

The baseline deliberately is *not* the caller's environment — on Windows it
comes from `CreateEnvironmentBlock`, on Unix it is rebuilt from the passwd
entry — so a variable only reaches the daemon if the allowlist admits it. That
allowlist is the whole `SOLDR_*` namespace plus a short list of individually
justified `ZCCACHE_*` names; see the constants near `FORWARDED_ENV_PREFIX` for
why each exception exists, and `tests/spawn_image.rs` for what is asserted to
cross and what is asserted to be dropped.
