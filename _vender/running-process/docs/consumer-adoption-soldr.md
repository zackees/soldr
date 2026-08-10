# soldr Consumer Adoption Guide

> **Status (#412): partially superseded.** The full-broker migration below
> (`Hello` negotiation, `ServiceDefinition`, `CacheManifest`) remains
> **deferred** under the minimal regime recorded in the
> [adoption dashboard](v1-consumer-adoption-dashboard.md). What soldr actually
> landed (soldr#721–724) is the direct-endpoint pattern: active `BackendHandle`
> identity probing, a local `soldr-daemon.servicedef`, a `daemon-identity` JSON
> sidecar, and `RUNNING_PROCESS_DISABLE=1` direct fallback — no broker, no
> Hello. When the deferred `connect_to_backend` work resumes it should target
> the `backend_sdk` `FrameClient`/`BackendEndpointMux`/`DaemonProcess` sidecar
> helpers and follow [INTEGRATE.md](INTEGRATE.md) instead of the steps below;
> this guide stays as the reference for the future full-broker wave.

This guide defines soldr-daemon adoption of the v1 running-process broker.
soldr already uses prost for its service payloads, so the migration focuses on
broker discovery, service registration, manifests, and rollback behavior.

## Target State

| Layer | soldr transition rule |
|---|---|
| service payloads | keep existing soldr prost messages and field numbers |
| broker control plane | use v1 `Frame`, `Hello`, `HelloReply`, and `Refused` |
| daemon endpoint | use the backend endpoint returned by the broker |
| rollback | use `RUNNING_PROCESS_DISABLE=1` and the direct soldr-daemon path |

The broker control-plane schema is documented in
[v1 wire envelope](v1-wire-envelope.md). The existing soldr prost payloads are
stored in `Frame.payload` for broker-backed requests.

## Migration Steps

1. Keep the existing soldr prost request and response definitions unchanged for
   the first broker adoption release.

2. Add a broker discovery layer before direct soldr-daemon discovery.

3. Add v1 `Hello` construction:

   - send `Hello.service_name = "soldr-daemon"`
   - send `Hello.client_min_protocol = 1`
   - send `Hello.client_max_protocol = 1`
   - set `Hello.wanted_version` to the soldr-daemon version
   - set `Hello.client_lib_name = "running-process"`

4. Use `HelloReply.Negotiated.backend_pipe` as the soldr-daemon endpoint for
   broker-backed requests.

5. Treat `HelloReply.Refused` as a structured setup error. Log
   `Refused.code`, `Refused.reason`, and `Refused.details`.

6. Register a soldr-daemon `ServiceDefinition` following
   [v1 service definition](v1-service-definition.md):

   ```textproto
   service_name: "soldr-daemon"
   isolation: SHARED_BROKER
   min_version: "0.8.0"
   labels {
     key: "consumer"
     value: "soldr"
   }
   ```

7. Publish a soldr `CacheManifest` following
   [v1 cache manifest](v1-cache-manifest.md). The manifest records state,
   pinned binary, runtime, lock, and log roots.

8. Add broker-backed tests for every soldr command that starts or contacts the
   daemon.

9. Keep direct soldr-daemon tests active until the escape hatch removal stage
   defined in [v1 rollout policy](v1-rollout-policy.md).

## Runtime Selection

soldr reads `RUNNING_PROCESS_DISABLE` before daemon discovery:

| Value | soldr behavior during transition |
|---|---|
| unset | use the release default recorded in soldr rollout metadata |
| `1` | use the direct soldr-daemon path |

The soldr diagnostics command prints the selected path, broker instance,
service definition path, manifest path, and daemon endpoint.

## Platform Behavior

soldr uses broker endpoint strings returned by running-process. It does not
derive platform pipe names locally.

| Platform | Broker endpoint behavior |
|---|---|
| Linux | Unix-domain socket under `$XDG_RUNTIME_DIR/running-process/broker`, with `/tmp/running-process-{uid}/broker` fallback |
| macOS | Unix-domain socket under `$TMPDIR/.rp-{uid}` with a 16-character hashed leaf |
| Windows | named pipe under `\\.\pipe\` |

soldr service definitions and manifests use the v1 platform directories:

| Platform | Service definition directory | Manifest registry |
|---|---|---|
| Linux | `$XDG_CONFIG_HOME/running-process/services` | `$XDG_DATA_HOME/running-process/manifests` |
| macOS | `~/Library/Application Support/running-process/services` | `~/Library/Application Support/running-process/manifests` |
| Windows | `%APPDATA%\running-process\services` | `%APPDATA%\running-process\manifests` |

## Compatibility Rules

- Existing soldr prost field numbers remain unchanged in the first broker
  adoption release.
- Broker protocol negotiation is separate from soldr service payload versioning.
- `ERROR_VERSION_UNSUPPORTED` means the running-process broker protocol range
  does not overlap with soldr's requested broker protocol range.
- `ERROR_VERSION_BLOCKED` means the broker service definition rejected the
  requested soldr-daemon version.
- `RUNNING_PROCESS_DISABLE=1` remains the supported rollback path through
  the documented rollout window.

## Test Matrix

- Existing soldr direct-path tests.
- Broker handshake tests for accepted and refused replies.
- Service definition validation tests for the soldr-daemon service name.
- Cache manifest round-trip tests for state, pinned binary, runtime, lock, and
  log roots.
- Per-platform endpoint tests for Linux, macOS, and Windows.
- Rollback tests with `RUNNING_PROCESS_DISABLE=1`.
