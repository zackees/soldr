# Daemon timeout and stall runbook

Cacheable compiler work has one mandatory route:

`Soldr front door → singleton broker → root/version daemon route → embedded zccache`

The front door registers the exact daemon image and cache root. Only the
broker places and starts the daemon. A wrapper re-entry never starts a broker
or daemon, and an infrastructure failure never silently switches to another
compiler path.

## First checks

Use these read-only commands before changing process state:

- `soldr doctor` shows effective timeouts and configuration.
- `soldr status` shows cache and daemon state.
- `soldr daemon status` checks the selected root's daemon.
- `soldr logs paths` prints the broker, daemon, build, and compile logs.

A long compile prints a progressive heartbeat rather than remaining silent.
The heartbeat names the operation, elapsed time, deadline, and the active
timeout override.

## Timeout surface

| Bound | Default | Override | Notes |
|---|---:|---|---|
| Broker front-door readiness | 2 s | — | Active control + SESSION probes, each capped at 250 ms; SQLite elects at most one starter |
| Broker daemon-route verdict | 5 s | — | One broker-owned child; no alternate route is attempted |
| SESSION setup | 6 s | `SOLDR_SESSION_ATTEMPT_BUDGET_MS` | Includes a 1 s delivery margin around the broker's 5 s route verdict |
| Status / shutdown reply | 2 s | — | Health handshakes should be immediate |
| Cache flush reply | 5 min | — | Large index/LTO flushes may be slow |
| Compile reply | 30 min | `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` | Shorten for diagnostic fail-fast behavior |
| Graceful shutdown wait | 5 min | — | Allows in-flight work and persistent state to drain |
| Cache shutdown | 5 min | `SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS` | End-of-command embedded-cache drain |

Malformed, empty, or zero timeout overrides use the documented default; they
do not remove the backstop. `soldr doctor` reports effective provenance.

## Failure modes

### Broker is unreachable

Signal: the wrapper reports that the singleton broker SESSION socket is not
available and tells you to use a Soldr build front door.

Confirm that the invocation began at `soldr cargo ...`, `soldr build`, or
another compile-capable Soldr surface. A raw compiler-wrapper invocation is
not authorized to create infrastructure.

Inspect the broker spawn log reported by `soldr logs paths`. The front door
uses active socket probes and a bounded wait; stale log lines cannot satisfy
readiness.

### Daemon route does not become ready

Signal: the broker cannot provide the registered service route within the
bounded acquisition window.

Inspect the broker and daemon spawn logs. They include the selected root,
route service name, registered image, and early child-exit reason. Do not
start a daemon manually: the broker is the only placement/spawn owner.

### Compile initialization is slow

The daemon binds its broker-facing endpoint and answers BackendHandle probes
before embedded zccache and database initialization complete. The first real
SESSION compile awaits compile-service readiness just in time. This keeps
startup probes responsive without allowing a compile to race initialization.

If the compile-reply heartbeat appears, use a smaller positive
`SOLDR_COMPILE_REPLY_TIMEOUT_SECS` for the next diagnostic run so it fails
quickly with evidence rather than waiting for the production backstop.

### Daemon is wedged

Run `soldr daemon stop`, wait for its bounded graceful shutdown, then run
`soldr daemon start`. Explicit start clears the stop tombstone, re-registers
the current daemon image, and asks the singleton broker to create the route.

Do not delete PID files, sockets, or `state.redb`; those are ownership and
forensic records, not recovery switches.

### Distinct cache roots interfere

This is a correctness bug. One install-path-scoped broker must route distinct
canonical cache roots/version/image identities to distinct daemon services.
Capture `soldr logs paths`, both roots, route names, PIDs, and socket paths.
Do not work around it by starting one broker per root.

## Recovery checklist

1. Inspect `soldr doctor`, `soldr daemon status`, and `soldr logs paths`.
2. Shorten the compile-reply timeout only when a bounded diagnostic failure is
   preferable to the normal long-build allowance.
3. Restart the selected broker-owned route with `soldr daemon stop` followed
   by `soldr daemon start`.
4. Re-run through a compile-capable Soldr front door.

All broker, daemon, transport, version-skew, retirement, initialization, and
protocol failures are hard failures for cacheable compiler work. There is no
rollout gate and no direct-daemon or direct-compiler fallback.
