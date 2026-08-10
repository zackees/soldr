# v1 Backend Lifecycle

The broker coordinates backend startup, recovery, idle shutdown, and process
tree cleanup. Backends still own service-specific work and data-plane RPC.

## Lifecycle States

| State | Meaning |
|---|---|
| `absent` | No live backend exists for the service/version. |
| `spawning` | One broker task holds the spawn lock. |
| `running` | Backend is ready and has a direct pipe. |
| `draining` | Backend is exiting after idle or shutdown policy. |
| `failed` | Backend failed and spawn budget controls retry. |

## Spawn Coordination

The broker serializes startup by service/version:

1. Acquire a per-service/version file lock.
2. Verify the lock file identity after acquisition.
3. Re-check the backend table under the lock.
4. Launch the backend under platform lifetime control.
5. Wait for the backend pipe to become ready.
6. Record the manifest and lifecycle event.
7. Release the lock.

The wait loop is adaptive and bounded. It keys progress to the spawning process
instead of sleeping for a fixed interval.

## Spawn Budget

Each service/version has a bounded spawn budget. Repeated crashes or failed
starts produce `ERROR_RATE_LIMITED` with `retry_after_ms`. A successful stable
backend replenishes the budget according to policy. The default budget allows
3 spawn attempts per 30-second window.

## Recovery

The broker treats these as stale backend entries:

- process id no longer exists
- process start identity does not match the manifest
- boot id differs from the current boot
- backend pipe is gone
- executable hash differs from manifest policy

Stale entries are removed before a new spawn attempt.

## Lifecycle Broadcasts

Phase 5 models broker control-plane fanout before backend RPC exists. The
broker can broadcast these lifecycle operations to every live backend:

- `release-handles`: maintenance asks backends to drop file handles below a
  path prefix before cleanup or replacement.
- `quiesce`: idle timeout, broker shutdown, or maintenance asks backends to
  stop accepting new work and drain.

The in-repo model tracks live targets, acknowledgements, timeouts, failures,
and dead backends skipped before fanout. Later backend RPC wiring should map
that model onto concrete backend requests without changing the Phase 5 result
shape.

## Idle Shutdown

Backends report or expose activity through their direct data plane. Phase 5
tracks each backend key's last activity with `std::time::Instant` and a
configured idle timeout. The default idle timeout is 30 seconds.

The broker-side idle model remains pure state: `mark_activity` resets a
backend's deadline, draining or quiesced backends are not emitted again, and
removed backends leave the idle table. When a running backend reaches its idle
deadline, `collect_due_for_quiesce` returns a due item containing the
`BackendKey`, elapsed idle duration, configured timeout, and
`QuiesceReason::IdleTimeout`. Later backend RPC wiring should feed those due
items into the lifecycle broadcast model's `quiesce` operation.

## Parent-Death Cleanup

| Platform | Phase 5 target | Broker behavior |
|---|---|---|
| Windows | Job object with kill-on-close and `CREATE_BREAKAWAY_FROM_JOB`. | Broker installs the job object target before exposing a backend pipe, unless the process is already inside a job. |
| Linux | Child-side `PR_SET_PDEATHSIG` with `SIGTERM`. | Broker installs the parent-death signal target before exposing a backend pipe. |
| macOS | kqueue supervisor child using `EVFILT_PROC` and `NOTE_EXIT`. | Broker reports the concrete `MacosKqueueSupervisorContract`. Backend spawn wiring must start the supervisor, register the broker PID with kqueue, re-check that the broker is still alive, and only then publish the backend pipe. On broker `NOTE_EXIT`, the supervisor sends `SIGKILL` to the backend within the 5-second cleanup budget. |

Other platforms remain `UnsupportedNoop` until a concrete cleanup primitive is
chosen. The broker starts lifetime control before it exposes a backend pipe.

### macOS kqueue Supervisor Contract

The Phase 5 macOS model treats the supervisor child as a concrete lifecycle
contract:

- The supervisor watches the broker parent PID.
- The supervisor registers `EVFILT_PROC` with `NOTE_EXIT` before the backend
  pipe is published.
- After registration, the supervisor re-checks that the broker is still alive
  to close the startup race where the broker exits before the kqueue watch is
  armed.
- When kqueue reports broker exit, the supervisor sends `SIGKILL` to the
  backend.
- The backend must be killed within 5 seconds of broker exit.

## Graceful Broker Shutdown

1. Stop accepting new `Hello` requests.
2. Return `ERROR_SHUTTING_DOWN` for in-flight startup work that has not
   committed a backend.
3. Leave already-routed client-to-backend connections alone.
4. Drop spawn locks.
5. Cancel partial child startup.
6. Drain for 10 seconds.
7. Force exit.

## Lifecycle Events

Every transition emits a bounded `LifecycleEvent`. See
[v1 lifecycle events](v1-lifecycle-events.md).
