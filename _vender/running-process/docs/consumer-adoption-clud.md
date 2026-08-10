# clud Consumer Adoption Guide

> **Status (#412): partially superseded.** The full-broker migration below
> (`Hello` negotiation, `ServiceDefinition`, `CacheManifest`) remains
> **deferred** under the minimal regime recorded in the
> [adoption dashboard](v1-consumer-adoption-dashboard.md). What clud actually
> landed (clud#319) is diagnostics-only (`clud daemon running-process --json`
> reports direct daemon fallback and `broker_client_wired: false`); clud's
> payload protocol `0x7C4C` is registered in
> `running_process::broker::protocol::registry`. When broker wiring resumes it
> should follow [INTEGRATE.md](INTEGRATE.md) and the `backend_sdk` module
> (`FrameClient`/`BackendEndpointMux`/`DaemonProcess` sidecar) instead of the
> steps below; this guide stays as the reference for the future full-broker
> wave.

This guide defines the clud migration from its legacy JSON control wire to the
v1 running-process prost broker wire.

## Target State

clud uses two wire modes during the transition:

| Mode | Encoding | Transport | Rollout use |
|---|---|---|---|
| `json-legacy` | JSON messages used by the existing clud direct path | direct clud daemon channel | default before broker rollout |
| `prost-v1` | running-process v1 `Frame`, `Hello`, `HelloReply`, and clud prost payloads | v1 broker socket or pipe | opt-in canary and final default |

The broker control plane always uses the schemas documented in
[v1 wire envelope](v1-wire-envelope.md). clud service payloads are encoded as
prost messages before insertion into the v1 frame `payload` field.

## Migration Steps

1. Add a `WireMode` selector at the clud daemon boundary:

   ```rust
   enum WireMode {
       JsonLegacy,
       ProstV1,
   }
   ```

2. Route every existing JSON request and response through a single legacy
   adapter. The adapter owns JSON serialization, JSON deserialization, and the
   conversion into the internal clud request model.

3. Add clud prost request and response messages with field names matching the
   internal request model. Use additive field numbers only. Reserve removed
   field numbers instead of reusing them.

4. Add a prost adapter beside the JSON adapter. The adapter owns
   `prost::Message::encode`, `prost::Message::decode`, and conversion into the
   same internal clud request model used by the JSON adapter.

5. Build a golden-message test set:

   - JSON request bytes
   - equivalent prost request bytes
   - expected internal request model
   - response model
   - equivalent JSON and prost response bytes

   The same test table runs through both adapters.

6. Add the v1 broker handshake for `prost-v1`:

   - send `Hello.service_name = "clud"`
   - send `Hello.client_min_protocol = 1`
   - send `Hello.client_max_protocol = 1`
   - send `Hello.client_lib_name = "running-process"`
   - set `Hello.wanted_version` to the clud daemon version

7. Use `HelloReply.Negotiated.backend_pipe` as the clud daemon endpoint. The
   direct JSON path keeps using the existing daemon endpoint.

8. Register a clud `ServiceDefinition` following
   [v1 service definition](v1-service-definition.md):

   ```textproto
   service_name: "clud"
   isolation: SHARED_BROKER
   min_version: "2.0.0"
   labels {
     key: "consumer"
     value: "clud"
   }
   ```

9. Publish a clud `CacheManifest` following
   [v1 cache manifest](v1-cache-manifest.md). The manifest records the clud
   runtime, lock, config, and log roots.

10. Keep both modes tested until the rollout policy removes the legacy JSON
    mode from the supported matrix.

## Runtime Selection

clud reads `RUNNING_PROCESS_DISABLE` before opening the daemon channel:

| Value | clud behavior during transition |
|---|---|
| unset | use the release default recorded in clud rollout metadata |
| `1` | use `json-legacy` and the direct daemon endpoint |

The clud command-line diagnostics report the selected wire mode, broker
instance, and daemon endpoint.

## Platform Behavior

clud does not build its own broker pipe names. It uses the v1 broker naming
contract in [v1 pipe naming](v1-pipe-naming.md):

| Platform | Broker endpoint behavior |
|---|---|
| Linux | Unix-domain socket under `$XDG_RUNTIME_DIR/running-process/broker`, with `/tmp/running-process-{uid}/broker` fallback |
| macOS | Unix-domain socket under `$TMPDIR/.rp-{uid}` with a 16-character hashed leaf |
| Windows | named pipe under `\\.\pipe\` |

Manifest registry locations follow [v1 platform behavior](v1-platform-behavior.md):

| Platform | clud manifest registry |
|---|---|
| Linux | `$XDG_DATA_HOME/running-process/manifests` |
| macOS | `~/Library/Application Support/running-process/manifests` |
| Windows | `%APPDATA%\running-process\manifests` |

## Compatibility Rules

- JSON request names remain stable for the full transition window.
- Prost field numbers are append-only.
- Unknown prost fields are preserved by forward-compatible readers when the
  generated types support preservation; otherwise they are ignored.
- `Refused.code = ERROR_VERSION_UNSUPPORTED` is a hard protocol mismatch.
- `Refused.code = ERROR_SERVICE_UNKNOWN` means the clud service definition is
  absent or invalid.
- `RUNNING_PROCESS_DISABLE=1` is the supported rollback path.

## Release Checklist

- Add adapter parity tests for every clud command.
- Add broker handshake tests for accepted and refused `Hello` replies.
- Add per-platform endpoint tests for Linux, macOS, and Windows.
- Add cache manifest round-trip tests for clud runtime and lock roots.
- Run direct JSON tests and broker prost tests in CI until JSON support is
  removed from the rollout matrix.
