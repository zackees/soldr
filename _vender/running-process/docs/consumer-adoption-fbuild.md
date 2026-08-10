# fbuild Consumer Adoption Guide

> **Status (#412): partially superseded.** The full-broker migration below
> remains **deferred** under the minimal regime recorded in the
> [adoption dashboard](v1-consumer-adoption-dashboard.md). What fbuild actually
> landed (fbuild#529/#530) is a service-metadata/direct-fallback seam plus a
> diagnostics-only `fbuild daemon running-process --json` preview — no
> running-process code dependency yet. The deferred active `BackendHandle`
> probe is the planned §6 measurement run for the second-pass SDK: add the
> `running-process` dep (features `client`, `backend-sdk`), probe via
> `probe_with_service_async` + a `DaemonProcess` identity sidecar, serve probes
> via `BackendEndpointMux`, and validate with the conformance kit per
> [INTEGRATE.md](INTEGRATE.md). This guide stays as the reference for the future
> full-broker wave.

This guide defines the fbuild adoption path for the v1 running-process broker.
The running-process repository does not contain fbuild source, so the first
fbuild adoption PR records the exact current fbuild wire and cache layout in
the fbuild repository before changing runtime behavior.

## Required Inventory

The first fbuild adoption PR records these facts in the fbuild repository:

| Area | Required record |
|---|---|
| daemon model | whether fbuild starts a persistent daemon, one worker per build, or direct child processes |
| request wire | current request encoding, protocol version constant, and compatibility window |
| response wire | current response encoding, error envelope, and retry behavior |
| cache roots | build artifact cache, index, temp, log, lock, and config roots |
| CI trust grouping | whether jobs share a user cache or require isolated instances |
| rollback path | command-line flag or environment variable that selects the direct path |

No external repository state is changed by this running-process documentation
PR.

## Target State

fbuild uses the v1 broker for daemon discovery and lifecycle management:

| Component | Target behavior |
|---|---|
| broker control plane | v1 `Frame`, `Hello`, `HelloReply`, and `Refused` |
| service payloads | prost messages defined by fbuild adoption PR |
| service name | `fbuild` |
| default isolation | `SHARED_BROKER` for per-user local builds |
| CI isolation | `EXPLICIT_INSTANCE` for trust-grouped CI jobs |
| rollback | `RUNNING_PROCESS_DISABLE=1` uses the direct fbuild path |

## Encoding Decision Table

The inventory result selects the migration lane:

| Current fbuild wire | Required transition |
|---|---|
| JSON | keep JSON direct path, add prost broker path, run parity tests for both encodings |
| bincode | freeze old protocol number, bump protocol number for prost, keep dual-read daemon support for the transition window |
| prost | keep existing field numbers, wrap payloads in the v1 broker frame, add broker handshake tests |
| custom binary | freeze old wire, define prost messages from the internal request model, keep direct custom wire through the transition window |

Every lane uses [v1 wire envelope](v1-wire-envelope.md) for broker
control-plane messages.

## Migration Steps

1. Complete the inventory table in the fbuild adoption PR.

2. Define a single internal request and response model used by both the direct
   path and the broker path.

3. Add prost service payload messages for broker-backed fbuild requests.
   Preserve current stable fields when fbuild already has a schema.

4. Add v1 `Hello` construction:

   - send `Hello.service_name = "fbuild"`
   - send `Hello.client_min_protocol = 1`
   - send `Hello.client_max_protocol = 1`
   - set `Hello.wanted_version` to the fbuild daemon or worker version
   - set `Hello.client_lib_name = "running-process"`

5. Use `HelloReply.Negotiated.backend_pipe` as the fbuild worker or daemon
   endpoint for broker-backed requests.

6. Treat `HelloReply.Refused` as a structured setup error. Log
   `Refused.code`, `Refused.reason`, and `Refused.details`.

7. Register an fbuild `ServiceDefinition` following
   [v1 service definition](v1-service-definition.md):

   ```textproto
   service_name: "fbuild"
   isolation: SHARED_BROKER
   min_version: "1.0.0"
   labels {
     key: "consumer"
     value: "fbuild"
   }
   ```

8. Use `EXPLICIT_INSTANCE` for CI jobs that intentionally isolate trust groups:

   ```textproto
   service_name: "fbuild"
   isolation: EXPLICIT_INSTANCE
   explicit_instance: "ci-trusted"
   min_version: "1.0.0"
   labels {
     key: "trust-domain"
     value: "ci-trusted"
   }
   ```

9. Publish an fbuild `CacheManifest` following
   [v1 cache manifest](v1-cache-manifest.md). The manifest records artifact,
   index, temp, log, lock, runtime, and config roots discovered during
   inventory.

10. Keep direct-path tests active until the escape hatch removal stage defined
    in [v1 rollout policy](v1-rollout-policy.md).

## Runtime Selection

fbuild reads `RUNNING_PROCESS_DISABLE` before worker or daemon discovery:

| Value | fbuild behavior during transition |
|---|---|
| unset | use the release default recorded in fbuild rollout metadata |
| `1` | use the direct fbuild path |

The fbuild diagnostics command prints the selected path, broker instance,
service definition path, manifest path, and worker or daemon endpoint.

## Platform Behavior

fbuild uses broker endpoint strings returned by running-process and follows the
platform contract in [v1 pipe naming](v1-pipe-naming.md):

| Platform | Broker endpoint behavior |
|---|---|
| Linux | Unix-domain socket under `$XDG_RUNTIME_DIR/running-process/broker`, with `/tmp/running-process-{uid}/broker` fallback |
| macOS | Unix-domain socket under `$TMPDIR/.rp-{uid}` with a 16-character hashed leaf |
| Windows | named pipe under `\\.\pipe\` |

Service definitions and manifests use the directories documented in
[v1 platform behavior](v1-platform-behavior.md):

| Platform | Service definition directory | Manifest registry |
|---|---|---|
| Linux | `$XDG_CONFIG_HOME/running-process/services` | `$XDG_DATA_HOME/running-process/manifests` |
| macOS | `~/Library/Application Support/running-process/services` | `~/Library/Application Support/running-process/manifests` |
| Windows | `%APPDATA%\running-process\services` | `%APPDATA%\running-process\manifests` |

## Test Matrix

- Inventory fixture tests for the existing fbuild wire.
- Golden-message parity tests for direct and broker paths.
- Broker handshake tests for accepted and refused replies.
- Service definition validation tests for `SHARED_BROKER` and
  `EXPLICIT_INSTANCE`.
- Cache manifest round-trip tests for artifact, index, temp, log, lock,
  runtime, and config roots.
- Per-platform endpoint tests for Linux, macOS, and Windows.
- Rollback tests with `RUNNING_PROCESS_DISABLE=1`.
