# Post-Mortem: Broker Daemon Service Integration (v1)

**Date:** 2026-06-12
**Scope:** Integration of the running-process broker/backend-lifecycle service
into consumer CLIs — zccache, soldr, fbuild, clud — through the "closed
minimal" regime (trackers zccache#698, soldr#718, fbuild#510, clud#308) and
the zccache FrameV1 transport lane (running-process#383).
**Audience:** maintainers planning a second-pass v1 API; AI agents performing
future consumer integrations.

---

## 1. Executive Summary

The broker v1 program shipped a sound protocol foundation (Frame envelope,
nonce-based `BackendHandle` identity probe, payload-protocol multiplexing) and
all four consumers reached a working minimal integration. But the cost per
consumer was high and dominated by **re-implementation of undifferentiated
plumbing**, not by consumer-specific logic. Every consumer hand-rolled the
same five things: endpoint byte-disambiguation, probe serving, Frame
construction, request-id correlation, and identity persistence. The library
exports the *protocol* but not the *integration surface*, so each adoption
became a multi-PR, multi-thousand-token archaeology project through ~17,400
words of design docs (24 files) and broker source (~13,400 LOC) to extract
maybe 300 lines of glue.

**Headline numbers:**

| Cost item | Evidence |
|---|---|
| zccache FrameV1 lane | 865 insertions across 9 files for one opaque-payload lane (running-process#383) |
| soldr probe adoption | 313-line bespoke `backend_handle_adoption.rs` + 3 follow-up PRs (soldr#721–724) |
| Doc corpus an integrator must triage | 24 `v1-*.md` / `consumer-adoption-*.md` files, ~17,400 words |
| Broker module surface | ~13,400 LOC across 30+ files; integration-relevant API scattered over 5 modules |
| PRs to reach "closed minimal" | soldr 4, zccache 2 (+1 Frame lane), fbuild 2, clud 1 — none reached full broker adoption |

The second-pass recommendation is a thin, opinionated **integration SDK**
(`backend_sdk` module or sub-crate) that collapses the five hand-rolled
pieces into ~10 lines of consumer code, plus a single-file quickstart that
replaces the doc corpus for the integration use case.

---

## 2. What Worked

### 2.1 The Frame envelope is genuinely good
`[u8 version=1][u32 LE len][prost Frame]` is simple, fixed, and pinnable.
The zccache golden-bytes tests froze exact wire bytes on the first attempt
and they decode bidirectionally with no surprises. `payload_protocol` as a
multiplexing key let zccache add a third wire lane to an endpoint already
serving two legacy formats **without breaking either** — that is the design
paying off exactly as intended.

### 2.2 BackendHandle probe semantics are the right abstraction
The verify-everything probe (endpoint tuple + nonce IPC round-trip + boot id
+ pid liveness + exe path + exe SHA-256) eliminates the entire class of
stale-PID-file / recycled-PID / swapped-binary bugs every daemon CLI
eventually hits. soldr and zccache both adopted it as their primary probe
with PID-file as degraded fallback, and the layered fallback story
(`RUNNING_PROCESS_DISABLE=1` → direct legacy path) held up in practice.

### 2.3 The escape hatch was honored consistently
`RUNNING_PROCESS_DISABLE=1` was implemented uniformly across all four
consumers and made the rollout reversible at every step. This is why
"closed minimal" was a safe stopping point instead of a half-migration trap.

### 2.4 Golden-bytes + mixed-wire testing pattern transfers
The test recipe pioneered in `crates/running-process/tests/broker/golden_bytes.rs`
(freeze exact bytes, then live round-trip, then mixed-protocol coexistence on
one endpoint) was copied into zccache's `daemon_wire_frame_v1_test.rs` and
caught the `FrameKind::Request = 0` proto3-omission subtlety immediately.
This should be productized (see §5.7).

### 2.5 Compile-time collision pinning
`const _: () = assert!(ZCCACHE_FRAME_PAYLOAD_PROTOCOL != BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL)`
style asserts make protocol-registry drift a build failure rather than a
runtime mystery. Cheap, effective, recommended as the documented pattern.

---

## 3. What Didn't Work

### 3.1 No server-side integration helper — every daemon re-implements the mux
This is the single largest friction source. running-process exports
`probe::handle_endpoint_probe<S: Read + Write>` — a **sync, exclusive-stream**
helper. Real consumers have **async tokio accept loops with their own
buffered readers and their own legacy wire formats sharing the endpoint**.
None could use the helper. Result: zccache's `try_serve_backend_handle_probe`
(~150 lines in `crates/zccache/src/ipc/transport.rs`) re-implements, by hand:

- `ensure_buffered` partial-read accumulation,
- byte-0 envelope-version detection,
- disambiguation against zccache's own `[len][ver=15|16]` headers
  (including the genuinely ambiguous case where a legacy body length of
  0x...01 makes byte 0 == 1),
- peek-decode of the Frame *without consuming* so non-probe frames fall
  through to the consumer's own dispatcher,
- `is_backend_handle_probe_request` validation (duplicating
  `validate_endpoint_probe_request_frame` internals),
- probe-response Frame construction (duplicating
  `endpoint_probe_response_frame`).

soldr's accept loop did its own equivalent. Every future consumer will too.
The disambiguation logic in particular is subtle, security-adjacent, and now
exists in N slightly-different copies.

### 3.2 Frame construction and correlation are pure boilerplate
To send one opaque payload, zccache had to build
`Frame { envelope_version: 1, kind, payload_protocol, payload, request_id, payload_encoding: None, deadline_unix_ms: 0, traceparent: "", tracestate: "" }`,
encode it, prepend the 5-byte header, enforce `MAX_FRAME_BYTES`, and invent a
per-connection `next_frame_request_id` wrapping counter — on both the client
and server connection types. None of this is zccache-specific. There is no
`Frame::request(...)` / `Frame::response_to(&req, ...)` constructor, no
buffer-level `encode_framed`/`decode_framed` (the `framing.rs` helpers only
take sync streams), and no client correlation primitive.

### 3.3 The payload-protocol registry is folklore
The constants live scattered: `0x0000` Hello (implicit), `0xAD01` admin,
`0xB232` probe (in `backend_lifecycle/probe.rs`), `0xD0FF` handoff. To pick
`0x7A63` for zccache, the integrating agent had to grep the **published crate
source inside `~/.cargo/registry`** to confirm non-collision. There is no
registry table in the docs, no reserved consumer range, no
`payload_protocol::is_reserved(u32)`. First-party and third-party values can
silently collide.

### 3.4 Sync/async API inconsistency
`BackendHandle::probe_with_service` is blocking; `BackendHandle::connect` is
async; probe *serving* helpers are sync `Read + Write`. Consumers on tokio
must wrap probes in `spawn_blocking` (zccache's mixed-wire test does exactly
this) and can't use the serve helpers at all. There is no async story for
the half of the API consumers actually call from async daemons.

### 3.5 Docs are rationale-heavy, integration-light, and partly stale
The 24-file, ~17,400-word v1 corpus is excellent *design rationale* but there
is no "integrate your daemon in 30 minutes" path. Worse, the prescriptive
guides drifted from what was actually built:
`consumer-adoption-zccache.md` still prescribes a full Hello/HelloReply
broker negotiation, `PROTOCOL_VERSION` bump, `ServiceDefinition`, and
`CacheManifest` registration — while the landed reality (zccache#708/709 +
the FrameV1 lane) is direct-endpoint probing plus an opaque Frame lane with
**no Hello, no broker, no manifest**. An agent following the guide literally
would build the wrong thing. For AI-agent integrators this is the token-cost
multiplier: the agent reads ~17k words to discover most of it doesn't apply
to the minimal regime.

### 3.6 Identity persistence reinvented per consumer
soldr wrote its own `daemon-identity.json` sidecar (write/read/remove +
malformed-file handling, ~40 lines + 4 tests). The `CacheManifest` registry
exists for exactly this but was heavyweight enough that no minimal-regime
consumer used it. There is no lightweight `DaemonProcess` ⇄ file helper in
between.

### 3.7 Platform foot-guns surface as code comments
The Windows endpoint rule — `Endpoint.path` is the **bare** pipe name because
`GenericNamespaced` prepends `\\.\pipe\` — is documented only in a comment
inside soldr's adoption module. Anyone constructing an `Endpoint` from a full
pipe path gets a silent mismatch. `Endpoint` has no smart constructors.

### 3.8 Process ceremony amplified the cost
Four tracker issues, per-consumer multi-PR sequences, a dashboard doc, phase
gates (#232/#235/#238/#239), and "closed minimal" status language — all for
integrations whose technical content is ~300 lines each. The ceremony was
appropriate for de-risking the *first* consumer; repeating it per consumer
without an SDK to amortize the cost is where the token spend ballooned.

---

## 4. Root Cause

**v1 shipped a protocol, not an integration surface.** The protocol is
stable, well-specified, and well-tested inside running-process. But the
boundary handed to consumers is "here are the prost types, the constants,
and 24 design docs — go." Each consumer therefore re-derived the same
integration layer from primary sources. For human teams that is slow; for
AI agents it is directly proportional to token cost, because the agent must
read the broker source and doc corpus into context for every integration,
then write and debug bespoke byte-level mux code that the library could have
provided once.

---

## 5. Recommendations: Second-Pass v1 API ("v1.1 Integration SDK")

Priority-ordered. Items 1–4 eliminate ~80% of observed consumer code.

### 5.1 `BackendEndpointMux` — the server-side one-liner (highest impact)
Ship in running-process (feature `backend-sdk`, tokio + sync variants):

```rust
let mux = BackendEndpointMux::new(daemon_identity)
    .with_legacy_detector(|buf| /* Some(Legacy) | Some(Frame) | NeedMoreBytes */)
    .with_payload_protocol(MY_PROTOCOL, |payload, ctx| async { /* -> response bytes */ });
// in the accept loop:
match mux.serve_step(&mut reader, &mut writer, &mut buf).await? {
    Served::Probe | Served::Handoff => continue,   // handled internally
    Served::Legacy => { /* consumer's existing path */ }
    Served::Payload(p) => { /* dispatched above */ }
}
```

Internally owns: buffered partial reads, envelope detection, the
legacy-header ambiguity rules, probe validation + response, handoff offers,
peek-without-consume, and `MAX_FRAME_BYTES` enforcement. This replaces the
~150-line hand-rolled block in every consumer and centralizes the
security-adjacent disambiguation logic in one audited place.

### 5.2 `FrameClient` — client-side correlation built in
`FrameClient::connect(&endpoint)` with `request(protocol, bytes) -> response bytes`
(internal request-id counter, response matching, deadline propagation,
timeout). Kills the per-consumer `next_frame_request_id` counters and
send/recv plumbing. Provide both async and blocking flavors.

### 5.3 Frame ergonomics + buffer-level codecs
`Frame::request(protocol, payload).with_request_id(n)`,
`Frame::response_to(&request, payload)` (echoes id + trace context),
and `encode_framed(&Frame) -> BytesMut` / `try_decode_framed(&mut BytesMut) -> Option<Frame>`
that work on buffers, not just `Read`/`Write` streams. Trace fields and
`payload_encoding` get correct defaults instead of being remembered (or
forgotten) per consumer.

### 5.4 Formal payload-protocol registry
One `broker::payload_protocol` module: every first-party constant, a
documented **consumer range** (e.g. `0x7000..=0x7EFF` first-come in a
registry table in one doc; `0xF000+` private-use, never registered),
`is_first_party(u32)`, and a `register_payload_protocol!` macro that emits
the compile-time collision asserts zccache wrote by hand. Add the table to
the quickstart doc.

### 5.5 Async-first identity APIs
`BackendHandle::probe_with_service_async(...)` (or make the whole type
async-first with `_blocking` variants). Document the `spawn_blocking`
requirement loudly wherever the sync probe remains.

### 5.6 `DaemonIdentityFile` helper + `Endpoint` smart constructors
`DaemonProcess::write_sidecar(path)` / `read_sidecar(path)` (atomic write,
tolerant read → `Option`) to replace soldr-style JSON sidecars, positioned
as the lightweight alternative to full `CacheManifest`. Add
`Endpoint::windows_pipe(name)` / `Endpoint::unix_socket(path)` constructors
that normalize the bare-pipe-name rule and reject `\\.\pipe\`-prefixed input.

### 5.7 Conformance test kit
A `running-process-conformance` dev-dependency (or test-support module)
exposing: golden-bytes assertion helpers for a consumer's payload protocol,
a `probe_responds_correctly(endpoint)` check, and a mixed-wire harness that
throws legacy + Frame + probe traffic at a consumer endpoint. Consumers get
the zccache test suite's coverage for ~30 lines instead of 378.

### 5.8 One-file integration quickstart; demote the corpus
Write `docs/INTEGRATE.md` (~600 words + 3 copy-paste snippets: serve, client,
conformance test) as the **only** document an integrator needs in the minimal
regime. Mark the `v1-*.md` corpus as design/reference. Update or clearly
deprecate `consumer-adoption-*.md` guides that prescribe the unlanded
Hello/broker path. For AI-agent integrations this is the single biggest
token reduction: context drops from ~17k words + broker source to one page
+ SDK rustdoc.

### 5.9 Lighten the process for SDK-backed adoptions
Once 5.1–5.7 exist, a consumer adoption is one PR: add dep, register
protocol, wire mux, run conformance kit. Replace the per-consumer
tracker/dashboard ceremony with a single checklist comment template. Keep
phase gates only for default-on rollout and escape-hatch removal (#238/#239),
which remain genuinely cross-cutting.

---

## 6. Sequencing Suggestion

1. **5.3 + 5.4** (Frame ergonomics, registry) — small, unblocks everything,
   no behavior change.
2. **5.1 + 5.2** (mux + client) — port zccache's FrameV1 lane onto them as
   the proving consumer; delete the hand-rolled transport block as the
   acceptance test.
3. **5.7 + 5.8** (conformance kit + quickstart) — written against the
   zccache port so the docs are demonstrably sufficient.
4. **5.5 + 5.6** — opportunistic; adopt in soldr when its deferred
   `connect_to_backend` work resumes.
5. Re-run one fresh consumer integration (fbuild's deferred active probe is
   the natural candidate) and measure: target is **one PR, < 100 consumer
   LOC, single-doc context** — versus the current 2–4 PRs, ~300–865 LOC, and
   24-doc corpus.

---

## 7. Per-Repo Change Plan (v1 is beta — change in place, no compat shims)

Because v1 has no deployed dependents, the items below modify the existing
API directly. File paths are current as of this writing.

### 7.1 zackees/running-process (the SDK — all new capability lands here)

**New module `crates/running-process/src/broker/backend_sdk/` (feature `backend-sdk`, default-on with `client`):**

| File | Contents |
|---|---|
| `mux.rs` | `BackendEndpointMux` (§5.1): tokio + sync variants. Absorb the disambiguation/peek/`ensure_buffered` logic currently duplicated in zccache `crates/zccache/src/ipc/transport.rs::try_serve_backend_handle_probe` and soldr `crates/soldr-cli/src/daemon/server.rs::answer_backend_handle_probe` (~line 634). Internally reuse `backend_lifecycle/probe.rs` validation rather than letting consumers reimplement `is_backend_handle_probe_request`. |
| `client.rs` | `FrameClient` (§5.2): connect, internal request-id counter, `request(protocol, bytes) -> bytes`, deadline/timeout, async + blocking. |
| `identity_file.rs` | `DaemonProcess::write_sidecar/read_sidecar/remove_sidecar` (§5.6) — lift soldr's `backend_handle_adoption.rs::{write,read,remove}_daemon_identity` semantics (tolerant read, atomic write). |

**Modify existing files:**

| File | Change |
|---|---|
| `src/broker/protocol/registry.rs` | Already the single registry (#375). Add: consumer ID range `pub const CONSUMER_PAYLOAD_PROTOCOL_RANGE: RangeInclusive<u32> = 0x7000..=0x7EFF`, private-use range `0xF000..`, `pub fn is_first_party(u32) -> bool`, and a `register_payload_protocol!` macro emitting the compile-time distinct-from-first-party asserts (the pattern zccache hand-wrote in `wire_frame.rs`). Append `0x7A63 zccache` to the registry doc table. |
| `src/broker/protocol/framing.rs` | Add buffer-level codecs (§5.3): `encode_framed(&Frame) -> BytesMut`, `try_decode_framed(&mut BytesMut) -> Result<Option<Frame>>` (existing helpers are sync-stream-only). |
| `src/broker/protocol/mod.rs` | Add `Frame::request(protocol, payload)` and `Frame::response_to(&req, payload)` constructors with correct defaults (envelope_version=1, encoding None, echo request_id + trace context). |
| `src/broker/backend_handle.rs` | Add `probe_with_service_async` / `probe_async` (§5.5); keep blocking variants with `_blocking` suffix or loud rustdoc on the `spawn_blocking` requirement. |
| `src/broker/backend_lifecycle/probe.rs` | Expose an async `handle_endpoint_probe` usable from a buffered tokio reader (or fold into `mux.rs` and re-export). |
| `src/broker/protocol/` (Endpoint) | `Endpoint::windows_pipe(name)` / `Endpoint::unix_socket(path)` smart constructors; reject `\\.\pipe\`-prefixed input (§5.6). |

**New test-support:** `crates/running-process/src/test_support/conformance.rs`
(or `publish = false` crate `crates/running-process-conformance/`):
golden-bytes assertion helpers, `probe_responds_correctly(endpoint)`,
mixed-wire harness (§5.7). Model on zccache's `daemon_wire_frame_v1_test.rs`
so consumers get its coverage in ~30 lines.

**Docs:** add `docs/INTEGRATE.md` one-pager (§5.8); mark `v1-*.md` as
reference; rewrite the stale Migration Steps in
`docs/consumer-adoption-zccache.md` (and audit the other three
`consumer-adoption-*.md`) to describe the landed probe + opaque-Frame-lane
pattern instead of the unlanded Hello/ServiceDefinition path; refresh
`docs/v1-consumer-adoption-dashboard.md` as consumers port.

### 7.2 zackees/zccache (proving consumer — port, then delete bespoke code)

| File | Change |
|---|---|
| `crates/zccache/src/ipc/transport.rs` | Delete `try_serve_backend_handle_probe`, probe-side `ensure_buffered`, `is_backend_handle_probe_request`, `backend_handle_probe_response`, `write_running_process_frame`; replace the accept-path call with `BackendEndpointMux` (legacy detector = existing v15/v16 header check). Drop `next_frame_request_id` counters and `send_frame_v1_request/response` once `FrameClient` owns correlation. |
| `crates/zccache/src/protocol/wire_frame.rs` | Shrink to `register_payload_protocol!(ZCCACHE_FRAME_PAYLOAD_PROTOCOL = 0x7A63)` plus zccache_v1 payload encode/decode; drop hand-rolled `buffer_starts_running_process_frame` / `encode_frame_v1_*` / `decode_frame_v1_message` in favor of SDK codecs. |
| `crates/zccache/src/daemon/server/connection.rs` | Replace `ResponseWire::FrameV1` plumbing with the mux payload-handler closure dispatching into the existing request handler. |
| `crates/zccache/tests/daemon_wire_frame_v1_test.rs` | Keep golden-bytes constants (wire must not change); replace the bespoke live/mixed-wire harness with the conformance kit. |

Acceptance: golden bytes identical before/after; net-negative consumer LOC.

### 7.3 zackees/soldr

| File | Change |
|---|---|
| `crates/soldr-cli/src/daemon/server.rs` | Replace `answer_backend_handle_probe` (~line 634) and the header byte-sniffing (~line 341) with `BackendEndpointMux`. |
| `crates/soldr-cli/src/daemon/backend_handle_adoption.rs` | Replace `write/read/remove_daemon_identity` with `DaemonProcess::*_sidecar`; build `soldr_daemon_endpoint` via the new `Endpoint` constructors (deletes the bare-pipe-name comment foot-gun); use the async probe variant where called from async context. |
| Deferred work unblocked | the `connect_to_backend` adoption deferred on soldr#718 should target `FrameClient` instead of raw streams. |

### 7.4 FastLED/fbuild

No `running_process` code dependency yet (only CI/dylint references) — the
#529/#530 work was a metadata/diagnostics seam. Implement the deferred
"active BackendHandle probe" directly against the SDK: add the
`running-process` dep (features `client`, `backend-sdk`), probe via
`probe_with_service_async` + identity sidecar in daemon discovery, serve
probes via `BackendEndpointMux` in the fbuild daemon accept loop, validate
with the conformance kit. This is the §6 measurement run: one PR, <100 LOC.

### 7.5 zackees/clud

Diagnostics-only today (clud#319: `clud daemon running-process --json`,
`broker_client_wired: false`). When broker wiring resumes in
`crates/clud-bin/src/daemon/server.rs`, follow the fbuild recipe (SDK dep,
sidecar + async probe, mux in the accept loop, conformance kit) and update
the diagnostics JSON to report `backend_sdk` adoption status instead of the
deferred-broker placeholder.

### 7.6 Cross-repo ordering

1. running-process: registry range + macro + Frame/framing ergonomics (no wire change).
2. running-process: mux + FrameClient + identity sidecar + async probe + conformance kit.
3. zccache port (proves the SDK; golden bytes must stay byte-identical).
4. soldr port (deletes the duplicate probe server + sidecar code).
5. `INTEGRATE.md` written against the zccache port; stale adoption docs fixed.
6. fbuild fresh integration as the measurement run (one PR, <100 LOC target).
7. clud when broker wiring resumes.

---

## 8. Appendix: Evidence Index

- zccache FrameV1 lane: branch `feat/frame-v1-transport` (zccache worktree
  issue-rp-383), commits `0cea66e`/`1501448`/`72ca43d`; hand-rolled mux in
  `crates/zccache/src/ipc/transport.rs` (`try_serve_backend_handle_probe`),
  frame codec in `crates/zccache/src/protocol/wire_frame.rs` (225 LOC).
- soldr probe adoption:
  `crates/soldr-cli/src/daemon/backend_handle_adoption.rs` (313 LOC),
  PRs soldr#721–724.
- Adoption status: `docs/v1-consumer-adoption-dashboard.md` ("closed
  minimal" across all four consumers; broker client, Hello negotiation,
  servicedef install, default-on all deferred).
- Library probe helpers consumers could not use as-is:
  `crates/running-process/src/broker/backend_lifecycle/probe.rs`
  (`handle_endpoint_probe` — sync, exclusive-stream).
- Doc corpus measurement: 24 files, ~17,400 words
  (`docs/v1-*.md` + `docs/consumer-adoption-*.md`).
- Stale guidance example: `docs/consumer-adoption-zccache.md` §"Migration
  Steps" (prescribes Hello/ServiceDefinition/CacheManifest path not used by
  the landed minimal regime).
