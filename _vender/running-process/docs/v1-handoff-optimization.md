# v1 Handoff Optimization

The v1 broker normally returns a backend pipe name and lets the client connect
directly. The handoff optimization shortens that path by transferring an
already-open backend connection handle when the platform supports it.

## Baseline Path

```text
client -> broker: Hello
broker -> client: Negotiated { backend_pipe }
client -> backend: connect(backend_pipe)
```

This path is always supported and remains the fallback.

## Optimized Path

```text
client -> broker: Hello
broker -> backend: open or reuse backend pipe
broker -> client: Negotiated { handle_passed_token }
broker -> backend: HandoffOffer (0xD0FF)   # duplicates the CLIENT connection
backend -> broker: HandoffAck (0xD0FF)
broker -> client: handoff-ready relay (EVENT frame, 0xD0FF, HandoffAck)
client: keeps its existing connection — now backend-served
```

The `backend_pipe` field remains present for diagnostics and fallback.

## Client Adoption Signaling

Client adoption is **verifiable and opt-in**
(`ConnectBackendRequest::adopt_handed_off_connection`, default `false`).
Because the broker-side handoff completes asynchronously after `Negotiated`
(offer → backend ACK), the client never adopts blindly: it waits — bounded by
`ConnectBackendRequest::handoff_ready_timeout` (default 2s) — for the broker's
handoff-ready relay on the same connection that carried Hello. The relay is a
broker→client push (`FRAME_KIND_EVENT`) under the `0xD0FF` handoff payload
protocol whose payload is the backend's `HandoffAck`; the client requires
`accepted = true` and a token echo matching its negotiated one-time
`handle_passed_token` (the only handoff secret the client knows — the
correlation id is broker↔backend bookkeeping). Brokers build the relay with
`handoff_ready_frame`.

On a confirmed relay the returned route is
`BackendConnectionRoute::HandlePassed` and the client keeps the socket it
already has; `BackendConnection::endpoint` still reports `backend_pipe` for
Hello-skip caching. Any failure — opt-out, missing capability bit, empty
token, relay timeout, refused or malformed relay, token mismatch — silently
downgrades to the baseline `backend_pipe` reconnect
(`BackendConnectionRoute::BrokerNegotiated`); adoption failure is never an
error by itself, and the bounded wait runs the blocking framed read on a
helper thread so the client can never hang on a silent broker.

## Backend Acceptance Helper

`running_process::broker::backend_lib::accept_handed_off` is the
platform-neutral backend scaffold for this path. Platform modules deliver
raw token bytes plus an opaque connection payload through `HandedOffPayload<T>`.
The helper parses the 128-bit token, consumes the matching pending token exactly
once, and classifies the payload as `Accepted` or `Rejected`.

The helper does not call `DuplicateHandle`, `sendmsg`, or `recvmsg`; those
transport details remain isolated in the Windows and Unix modules.

## Platform Transport Scaffold

`running_process::broker::server::handoff::windows` models `DuplicateHandle`
attempt inputs, result, and fallback-safe errors.
`running_process::broker::server::handoff::unix` does the same for
`SCM_RIGHTS`. The platform modules own the direct handle/file-descriptor
transfer attempt when compiled on their target OS, while unsupported,
permission-denied, and timeout-like outcomes are translated into the existing
silent reconnect fallback policy.

## Platform Mechanisms

| Platform | Mechanism |
|---|---|
| Windows | `DuplicateHandle` into the backend process. |
| Linux | `SCM_RIGHTS` over Unix-domain socket. |
| macOS | `SCM_RIGHTS` over Unix-domain socket. |

## Cross-OS Acceptance Evidence

Phase 6 handoff changes must keep a platform-specific acceptance trail instead
of relying on one OS to prove all transports. Each platform runs:

```text
soldr cargo test -p running-process --test broker --features client handoff -- --nocapture
soldr cargo test -p running-process --test broker --features client windows_duplicate_handle_passes_pipe_to_child_process -- --nocapture
```

| Platform | Transport expectation | Acceptance evidence |
|---|---|---|
| Windows | `DuplicateHandle` enabled, `SCM_RIGHTS` disabled | `DUPLICATE_HANDLE_TRANSPORT_SUPPORTED` matches the Windows target; fallback tests prove permission, integrity, and ack-timeout failures stay silent reconnects. `handoff_windows_duplicate_handle::windows_duplicate_handle_passes_pipe_to_child_process` duplicates a real pipe handle into a child process, closes the broker-owned read handle, and proves the child reads bytes while echoing the paired 128-bit handoff token. |
| Linux | `SCM_RIGHTS` enabled, `DuplicateHandle` disabled | `SCM_RIGHTS_TRANSPORT_SUPPORTED` matches the Unix target; Unix transport tests pass a descriptor and 128-bit token through the handoff socket. |
| macOS | `SCM_RIGHTS` enabled, `DuplicateHandle` disabled | The same Unix transport and fallback evidence must pass on macOS so the optimization does not rely on Linux-only socket behavior. |

## End-to-End Acceptance Evidence

`handoff_end_to_end_acceptance` pins the platform-neutral success contract that
Phase 6 must preserve once the production client/broker/backend path is wired:

- `HandoffFallbackState` must allow an enabled backend to attempt handoff.
- `DuplicateHandleSuccess` and `ScmRightsSuccess` must carry the same 128-bit
  token that the backend is expecting.
- `accept_handed_off` must accept each transport payload exactly once, consume
  the pending token, and reject replay with `TokenNotPending`.
- `TokenMismatch` must reject the transport payload, leave the pending token
  retryable, and map the broker attempt to the existing silent reconnect
  fallback.

This is acceptance evidence for the handoff control contract. It does not close
Phase 6 by itself; final acceptance still requires real client-to-broker-to-backend
smoke evidence for Windows `DuplicateHandle`, Linux/macOS `SCM_RIGHTS`, and
measured latency against the reconnect fallback.

## Fallback Triggers

The broker returns the baseline `backend_pipe` path when:

- peer credentials cannot be mapped to a process handle
- handle duplication fails
- Unix socket ancillary data transfer fails
- service policy disables handoff
- client capabilities do not include handoff support
- broker is under resource pressure
- the broker adopted an existing backend after restart

Fallback is not a protocol error.

## Adopted Existing Backends

When a broker restart discovers a live backend through the central manifest, the
new broker treats that backend as adopted. It cannot use `DuplicateHandle` or
`SCM_RIGHTS` to transfer a connection that was accepted by the old broker, so it
must use reconnect mode for that backend.

The expected negotiated reply for an adopted backend keeps `backend_pipe`
populated, where `handle_passed_token` is empty. In code this is represented by
`HandoffFallbackReason::AdoptedBackend`, and the client follows the same
baseline reconnect path as any other fallback.

## Latency Proof

`running_process::broker::server::handoff::compare_handoff_latency` compares
handoff samples against reconnect fallback samples at P50 and P99. The helper
requires handoff to be strictly faster at both percentiles, preventing Phase 6
from claiming equal or slower handle passing as a successful optimization. The
registered `handoff_latency` tests cover both the faster case and the rejection
of equal or slower handoff samples.

### Measured Real-Path Evidence

`handoff_latency_e2e` measures the two real paths and prints P50/P99 evidence
into the test output on every run:

- **handoff**: the completed #367/#368 orchestration — one-time token issue +
  ACK registration, the real platform transport (`DuplicateHandle` into a live
  child process on Windows over the child-helper line protocol, or
  `sendmsg(SCM_RIGHTS)` through a real `UnixListener` handoff socket on Unix),
  backend payload adoption, and token-echo acknowledgement.
- **reconnect**: the fallback the handoff replaces — a fresh client
  `connect_to_backend` Hello-skip local-socket connect to the cached
  `backend_pipe` plus the first payload write.

Methodology: `collect_latency_samples` (5 warmup + 50 measured iterations,
monotonic `Instant` timing, per-iteration setup excluded from the timed
region) summarized by `summarize_latency_samples` at nearest-rank P50/P99.
The tests assert sample sanity (all iterations collected, non-zero,
P50 <= P99) and deliberately do **not** assert that handoff is faster, so CI
scheduler noise cannot flake them; the printed numbers are the evidence.

Reproduce with:

```text
soldr cargo test -p running-process --features client --test broker -- latency --nocapture
```

Measured on 2026-06-11 (debug builds; Windows 10 host, Linux under Docker
`rust:1.85` on the same host):

| Platform | Handoff P50 | Handoff P99 | Reconnect P50 | Reconnect P99 |
|---|---|---|---|---|
| Windows (`DuplicateHandle`) | 78 us | 130 us | 38 us | 53 us |
| Linux (`SCM_RIGHTS`) | 72 us | 156 us | 54 us | 463 us |

Honest reading: at P50 the bare reconnect connect is currently cheaper on both
platforms because the measured handoff still pays a broker-to-backend
delivery/ACK round trip per handoff (the stand-in for the future wire frame).
At P99 the Linux `SCM_RIGHTS` path is already ~3x tighter than reconnect
(156 us vs 463 us), which is where the optimization earns its keep: the
handoff latency distribution is narrow and predictable, while fresh connects
carry a heavy tail. The Windows delivery channel in this harness is a child
stdin/stdout line protocol, so its numbers are an upper bound for a real wire
frame.

### Serve-path evidence (production wiring, 2026-06-11)

The harness numbers above predate the serve-side wiring. With #387 merged,
`handoff_serve_latency::serve_path_handoff_vs_reconnect_latency_evidence`
measures the same comparison through the REAL `serve_registered_backend`
accept loop: an opted-in `connect_to_backend` against a serve config with
`with_handoff_endpoint` (full Hello → platform handoff → offer/ACK wire
frames → handoff-ready relay → adoption) versus a non-opted-in client
against the same serve loop with handoff disabled (full Hello →
`backend_pipe` reconnect — the production default). Both timed regions end
after one probe/reply byte round trip on the resulting backend connection,
so every sample proves the route serves traffic. Same methodology: 5
warmup + 50 measured iterations, nearest-rank P50/P99.

Reproduce with:

```text
soldr cargo test -p running-process --features client --test broker serve_path_handoff_vs_reconnect_latency_evidence -- --nocapture
```

Measured on 2026-06-11 (debug builds):

| Platform | Handoff P50 | Handoff P99 | Reconnect P50 | Reconnect P99 |
|---|---|---|---|---|
| Windows (`DuplicateHandle`, production serve loop) | 495 us | 1076 us | 377 us | 571 us |
| Linux (`SCM_RIGHTS`, production serve loop, Dockerized) | 575 us | 830 us | 207 us | 610 us |
| macOS (`SCM_RIGHTS`, production serve loop) | pending CI runner | pending CI runner | pending CI runner | pending CI runner |

The Windows numbers were stable across three runs (handoff P50 482–544 us,
P99 892–1641 us; reconnect P50 302–403 us, P99 534–682 us; the table records
the median run). The Linux numbers were measured 2026-06-11 inside Docker
(`rust:latest` glibc container with the repo-pinned toolchain, debug
profile, same Windows 10 host) and were likewise stable across three runs
(handoff P50 564–629 us, P99 828–1550 us; reconnect P50 207–274 us,
P99 482–633 us; the table records the median run). The two serve-path
`handoff_serve_e2e` tests (adopt + rejected-downgrade) pass in the same
container, confirming the production `SCM_RIGHTS` path on Linux.

The macOS row stays pending: as of 2026-06-11 the macOS unit-test CI jobs
(ARM and x86) abort before reaching this benchmark because
`handoff_serve_e2e::rejected_handoff_silently_downgrades_to_backend_pipe`
fails on macOS runners — the broker serve socket never binds and the
client retries time out with `No such file or directory`. That
macOS-specific failure needs its own investigation before serve-path
numbers can be captured there.

Honest reading: through the production serve path the
handoff route pays the full Hello **plus** the broker→backend handoff dial,
`DuplicateHandle` (Windows) or `sendmsg(SCM_RIGHTS)` (Linux), offer/ACK
frames, and the handoff-ready relay — all serialized in the broker accept
loop — while reconnect pays only Hello plus one extra local-socket connect.
On both Windows and Linux that makes reconnect faster at both P50 and P99;
the P99-tail advantage that `SCM_RIGHTS` showed in the pre-wiring Linux
harness does **not** survive the serve path (830 us handoff vs 610 us
reconnect at P99).

### Out-of-process rollout evidence (production deployment, 2026-06-12)

The serve-path numbers above still ran the serve loop and the client in
one test binary. The `handoff_rollout_evidence` example
(`crates/running-process/examples/handoff_rollout_evidence.rs`) closes
the remaining gap by deploying the production topology as real separate
OS processes: a **server process** running the production
`serve_registered_backend` accept loop (the same function
`running-process-broker-v1 --serve` invokes) together with the backend
side (startup endpoint-identity probe, production offer/ACK handoff wire
exchange or `backend_pipe` reconnect listener), and a **client process**
performing the opt-in `connect_to_backend` across the process boundary.
Every timed sample asserts the expected route
(`HandlePassed` / `BrokerNegotiated`) and ends with a probe/reply byte
round trip on the resulting backend connection. Same methodology: 5
warmup + 50 measured iterations, nearest-rank P50/P99.

**Topology constraint discovered during the rollout attempt:** the
registered-backend serve mode verifies its startup endpoint probe
against the serving process's OWN identity (pid / exe_path /
exe_sha256 in `build_registered_backend`, serve.rs). A standalone
`running-process-broker-v1 --serve` process fronting a *separate*
backend process therefore fails startup with
`endpoint probe response identity did not match expected daemon
identity: pid`. The supported production deployment co-locates the
serve loop in the backend service process; only the client is a foreign
process. (Cross-process `DuplicateHandle` into a distinct backend pid
remains separately evidenced by
`handoff_latency_e2e::windows_bench`, which duplicates into a live
child process.) A fully split broker/backend topology would need an
identity-verification story first and is out of scope for #387.

Reproduce with:

```text
soldr cargo build -p running-process --features client --example handoff_rollout_evidence
target/<triple>/debug/examples/handoff_rollout_evidence driver
```

Measured on 2026-06-12 (debug builds, rustc 1.94.1; AMD Ryzen 7 3700X
desktop, Windows 10 Pro host; Linux inside the
`running-process/linux-lint:local` Docker container, Debian 12 glibc on
WSL2 kernel 6.18, same host):

| Platform | Handoff P50 | Handoff P99 | Reconnect P50 | Reconnect P99 |
|---|---|---|---|---|
| Windows (`DuplicateHandle`, out-of-process client) | 428 us | 652 us | 312 us | 400 us |
| Linux (`SCM_RIGHTS`, out-of-process client, Dockerized) | 522 us | 904 us | 244 us | 447 us |

Stable across runs — Windows (4 runs): handoff P50 425–447 us, P99
652–844 us; reconnect P50 301–315 us, P99 400–533 us. Linux (3 runs):
handoff P50 502–535 us, P99 904–1228 us; reconnect P50 244–316 us, P99
447–567 us. Each table row records the median run.

Honest reading: the out-of-process numbers reproduce the in-process
serve-path result almost exactly — reconnect is faster at both P50 and
P99 on both platforms, by roughly the same margins (the per-connection
broker→backend offer/ACK round trip plus the handoff-ready relay costs
more than the fresh `backend_pipe` connect it replaces). Crossing a
real process boundary adds no surprises in either direction, and all
50×2×2×2 measured connections adopted (or reconnected) on the expected
route with zero fallbacks.

### Default-policy decision (Phase 7)

**Handoff stays opt-in.** Phase 7 does not flip
`BrokerServeConfig::handoff_endpoint` (or the client's
`adopt_handed_off_connection`) to default-on.

Justification: the latency gate for making handoff the default has always
been `compare_handoff_latency`'s contract — strictly faster at both P50 and
P99 — and the measured evidence fails it. The pre-wiring harness already
showed reconnect cheaper at P50 on both Windows and Linux, and the new
serve-path numbers above show that through the production wiring on Windows
reconnect is faster at **both** percentiles (377 us vs 495 us at P50, 571 us
vs 1076 us at P99): the per-connection broker→backend offer/ACK round trip
costs more than the fresh `backend_pipe` connect it replaces. The last
open case for handoff was the tail-latency tightness `SCM_RIGHTS` showed in
the pre-wiring Linux harness (P99 156 us vs 463 us), but the Dockerized
Linux serve-path run did **not** reproduce it: reconnect is faster at both
percentiles on Linux too (207 us vs 575 us at P50, 610 us vs 830 us at
P99), so the Linux evidence now confirms the stay-opt-in decision rather
than challenging it. The 2026-06-12 out-of-process rollout evidence
(real server and client processes, table above) re-confirms the same
verdict on both platforms with production-shaped deployments: reconnect
wins at both percentiles on Windows (312 us vs 428 us at P50, 400 us vs
652 us at P99) and on Linux (244 us vs 522 us at P50, 447 us vs 904 us
at P99), with every opted-in connection adopting cleanly — the
mechanism is sound; it is just not faster than the fallback it
replaces. **Decision (2026-06-12): handoff remains opt-in in Phase 7;
the reconnect fallback stays the authoritative correctness path.**
Until a platform shows a
strictly-faster serve-path result, the optimization remains available to
operators who opt in via `--handoff-endpoint` and clients that opt in via
`adopt_handed_off_connection`, with the reconnect path staying the
authoritative default.

## Operations: enabling the serve-path handoff

Serve mode keeps handoff off unless the broker is started with the backend
handoff endpoint (see `docs/v1-broker-architecture.md` for the full serve
usage):

```bash
running-process-broker-v1 --serve <socket-path-or-pipe-name> \
  --service zccache \
  --version 1.11.20 \
  --backend-endpoint <backend-socket-or-pipe> \
  --handoff-endpoint <backend-handoff-socket-or-pipe>
```

`--handoff-endpoint <path>` maps to
`BrokerServeConfig::with_handoff_endpoint` and names the endpoint the
backend listens on for the broker's `HandoffOffer`/`HandoffAck` exchange.
Omitting the flag (the default) disables handoff entirely: negotiated
clients always reconnect through `--backend-endpoint`. Handoff failures
with the flag set are silent optimization failures — clients fall back to
the reconnect path with no client-visible error.

## Capability Bits

Handoff is negotiated through additive capability bits in `Hello` and
`Negotiated`. A client that does not advertise handoff support receives the
baseline response.

## Tuning

Operators tune handoff by service definition labels and broker config:

| Setting | Effect |
|---|---|
| `handoff.enabled` | Enables or disables the optimization. |
| `handoff.max_attempts_per_minute` | Bounds failed handoff work. |
| `handoff.disable_under_fd_pressure` | Forces baseline path during FD pressure. |

Metric and event names are stable. See [v1 observability](v1-observability.md).
