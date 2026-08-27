# `lifecycle`

Daemon process lifecycle: route-claim liveness checks, the ownership and
displacement policy for stale daemon generations, and structured lifecycle
events. The stable broker owns daemon process creation.

`mod.rs` was formerly `lifecycle.rs`. It was converted to a directory when it
crossed the repository's 1,500-line hard ceiling.

| File | Holds |
|---|---|
| `mod.rs` | route-claim liveness, ownership, displacement, and lifecycle events |
| `spawn_env.rs` | the environment forwarded to a broker-launched daemon |
| `tests/` | the three former inline test modules, one file each |

## Spawn environment

`spawn_env.rs` supplies the narrow environment overlay that the stable broker
uses when it launches a daemon generation. Composition remains separate from
the broker's process-creation boundary.

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
