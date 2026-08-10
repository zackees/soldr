# v1 Broker Internal Architecture

This document is for contributors working on `running-process-broker-v1`.
The public architecture is summarized in
[v1 architecture overview](v1-architecture-overview.md).

## Process Model

`running-process-broker-v1` is a per-user local IPC daemon. Each broker
instance owns one trust domain:

- shared user broker
- private service broker
- explicit named instance

The broker accepts control-plane connections, validates `Hello`, coordinates
backend lifecycle, and returns direct backend pipe addresses.

## Major Components

| Component | Responsibility |
|---|---|
| Listener | Binds the platform pipe or socket for one broker instance. |
| Framing | Reads and writes `[1][u32 length][prost body]` frames. |
| Protocol | Decodes `Frame`, `Hello`, `HelloReply`, and admin payloads. |
| Service registry | Loads and validates service-definition files, re-reading on `Hello`. |
| Instance resolver | Maps service definitions to shared, private, or explicit broker instances. |
| Rate limiter | Bounds Hello requests per verified peer PID. |
| Backend endpoint allocator | Generates unguessable backend pipe endpoints and avoids duplicate allocations. |
| Backend registry | Tracks verified backend handles by instance, service, and version. |
| Spawn coordinator | Serializes backend startup for one service/version. |
| Perf guard | Summarizes Hello latency samples and enforces the frozen P50/P99 budgets. |
| Lifecycle monitor | Watches process death, idle timers, and shutdown drains. |
| Manifest registry | Reads and writes central cache manifests. |
| Admin dispatcher | Implements status, dump, health, config, diagnose, and metrics verbs. |
| Event log | Appends bounded `LifecycleEvent` records. |

## Hello Path

1. Listener accepts one local IPC connection.
2. The accept loop derives platform peer credentials and applies the broker
   peer policy. Rejected peers are dropped silently before any `Hello` bytes
   are read or any `HelloReply` is written.
3. `server::connection::handle_hello_connection` reads the initial frame with
   the 64 KiB `Hello` cap.
4. Protocol layer decodes `Frame`, verifies it is a control-plane request,
   then decodes `Hello` from `Frame.payload`.
5. Service registry resolves and revalidates the service definition.
6. Instance resolver selects the broker trust domain.
7. Backend registry returns a live backend or asks the spawn coordinator to start
   one.
8. Broker writes a response `Frame` whose payload is `HelloReply`
   (`Negotiated` or `Refused`).
9. Client disconnects and uses the backend pipe directly.

The first server slices expose this boundary as `HelloRequest` and
`handle_hello_connection`: the request contains the decoded `Hello`, the
original `Frame` metadata, and the OS-verified peer identity. The framed I/O
boundary also exposes `handle_hello_connection_with`, which accepts any
`HelloResponder`; focused tests can use the in-memory `HelloHandler`, while the
broker accept loop can route the same wire frame through `HelloRouter`. The
control-socket accept loop calls the same connection handler after binding the
platform socket and checking credentials.

The serve-mode slice wires this path end-to-end for an already-known backend
endpoint:

```bash
running-process-broker-v1 --serve <socket-path-or-pipe-name> \
  --service zccache \
  --version 1.11.20 \
  --backend-endpoint <backend-socket-or-pipe>
```

This mode loads `<service>.servicedef`, resolves the broker instance, routes the
provided endpoint through the backend registry, then serves Hello and admin
frames through `HelloRouter` until the process exits. Tests and harnesses may
pass `--max-connections <n>` to request a bounded run. Service definitions are
still reloaded for each accepted Hello, so policy changes made after binding
are reflected in replies. The serve path uses the live registry mode and prunes
stale backend handles before each lookup. It uses current-process backend
identity as a temporary bridge; spawn-managed backend identity remains the
responsibility of the later spawn coordinator slice.

### `--handoff-endpoint` (opt-in handle-passing handoff)

Passing `--handoff-endpoint <backend-handoff-socket-or-pipe>` opts the serve
loop into the handle-passing handoff (#387,
[v1 handoff optimization](v1-handoff-optimization.md)). After a Hello that
negotiated handle passing, the broker dials this endpoint, transfers the
client connection to the backend (`DuplicateHandle` on Windows,
`SCM_RIGHTS` on Unix) paired with the one-time token, and relays the
handoff-ready event to the client. Omitting the flag — the default —
disables handoff entirely; negotiated clients always reconnect through
`--backend-endpoint`. Handoff failures are never client-visible errors:
clients silently fall back to the reconnect path. Per the measured
serve-path latency evidence, handoff remains opt-in (see the
default-policy decision in the handoff doc).

`server::HelloRouter` is the broker-side routing layer for this path. It
reloads `<service>.servicedef` for each request, checks the version policy,
resolves the trust-domain instance, and turns backend-registry misses into the
stable spawn-failed placeholder until real spawn-on-Hello is wired in. When a
spawn coordinator is attached, repeated misses consume the per-backend-key spawn
budget and return `ERROR_RATE_LIMITED` with a retry hint once the budget is
exhausted.

The backend registry exposes `prune_stale`; the lifecycle monitor calls it
before live Hello routing returns a negotiated endpoint.

## Backend Table

The backend registry is keyed by:

```text
(broker_instance, service_name, service_version)
```

Each entry stores:

- backend process id
- backend pipe endpoint
- backend executable hash
- boot id
- last activity timestamp
- spawn budget state
- lifecycle event cursor

## Concurrency Rules

- One spawn lock exists per service/version.
- A per-instance/service/version spawn budget allows 3 attempts per 30-second
  window by default.
- Only one spawn attempt may be in flight for a backend key at a time; duplicate
  attempts receive an in-progress error instead of starting another child.
- Failed spawn attempts consume budget until the window resets. A successful
  spawn resets the budget for that backend key.
- The broker never holds the global backend table lock while launching a child
  process.
- Admin dump snapshots copy state into a diagnostic structure before encoding
  JSON.
- Shutdown cancellation is broadcast before listener close.

## Error Mapping

Internal errors map to stable `Refused.code` values:

| Internal condition | Wire code |
|---|---|
| Protocol range mismatch | `ERROR_VERSION_UNSUPPORTED` |
| Missing service definition | `ERROR_SERVICE_UNKNOWN` |
| Spawn failure | `ERROR_BACKEND_SPAWN_FAILED` |
| Spawn budget exhausted | `ERROR_RATE_LIMITED` |
| Shutdown in progress | `ERROR_SHUTTING_DOWN` |
| Peer credential failure | `ERROR_PEER_REJECTED` |
| Policy version floor | `ERROR_VERSION_BLOCKED` |
| Unclassified invariant failure | `ERROR_INTERNAL` |

## Contributor Rule

Code changes that affect any component above update the matching v1 document in
the same PR.
