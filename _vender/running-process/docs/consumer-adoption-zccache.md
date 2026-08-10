# zccache Consumer Adoption Guide

> **Status (#412): partially superseded.** The migration steps below
> describe the eventual full-broker path (`Hello` negotiation,
> `ServiceDefinition`, `CacheManifest`), which remains **deferred**
> under the minimal regime recorded in the
> [adoption dashboard](v1-consumer-adoption-dashboard.md). What zccache
> actually landed is the direct-endpoint pattern: `BackendHandle`
> identity probing plus an opaque Frame lane
> (payload protocol `0x7A63`, registered in
> `running_process::broker::protocol::registry`) on zccache's own
> daemon endpoint — no broker, no Hello. New integrations should follow
> [INTEGRATE.md](INTEGRATE.md) and the `backend_sdk` module instead of
> the steps below; this guide stays as the reference for the future
> full-broker wave.

This guide defines the zccache migration from its legacy bincode wire to the v1
running-process prost broker wire.

## Target State

zccache uses a single internal request model with two external encodings during
the transition:

| Mode | Encoding | Transport | Rollout use |
|---|---|---|---|
| `bincode-legacy` | existing zccache bincode payloads | direct zccache daemon channel | compatibility window |
| `prost-v1` | running-process v1 `Frame`, `Hello`, `HelloReply`, and zccache prost payloads | v1 broker socket or pipe | canary and final default |

The broker control plane always uses [v1 wire envelope](v1-wire-envelope.md).
zccache service payloads are prost bytes stored in `Frame.payload`.

## PROTOCOL_VERSION Strategy

The first zccache release that accepts prost payloads bumps
`PROTOCOL_VERSION` to the next integer after the last bincode-only protocol.

| Release lane | Accepted client payloads | Daemon response payload |
|---|---|---|
| last bincode-only release | bincode at the old `PROTOCOL_VERSION` | bincode |
| transition release | bincode at the old version and prost at the new version | same encoding as request |
| broker-default release | prost at the new version | prost |

Rules:

- The bincode protocol number is frozen after the prost transition starts.
- The prost protocol number is used for every broker-backed zccache request.
- A daemon that receives an unsupported protocol number returns a stable
  protocol mismatch error before reading the service payload.
- Rollback uses `RUNNING_PROCESS_DISABLE=1` and the bincode direct path.

## Migration Steps

1. Move all bincode encode and decode calls into `BincodeLegacyCodec`.

2. Add `ProstV1Codec` beside the legacy codec. It encodes and decodes the same
   internal request and response model.

3. Define zccache prost messages with stable numeric field tags. Reserve all
   removed field numbers and names.

4. Add a protocol detection boundary before payload decode:

   ```rust
   enum ZccacheWire {
       BincodeLegacy { protocol_version: u32 },
       ProstV1 { protocol_version: u32 },
   }
   ```

5. Reject unknown protocol numbers before request dispatch.

6. Add dual-read daemon support for the transition release. The daemon accepts
   the old bincode protocol and the new prost protocol on the direct endpoint.

7. Add broker handshake support for the prost lane:

   - send `Hello.service_name = "zccache"`
   - send `Hello.client_min_protocol = 1`
   - send `Hello.client_max_protocol = 1`
   - set `Hello.wanted_version` to the zccache daemon version
   - use `HelloReply.Negotiated.backend_pipe` as the broker-selected endpoint

8. Register a zccache `ServiceDefinition` following
   [v1 service definition](v1-service-definition.md):

   ```textproto
   service_name: "zccache"
   isolation: SHARED_BROKER
   min_version: "1.11.20"
   labels {
     key: "consumer"
     value: "zccache"
   }
   ```

9. Publish a zccache `CacheManifest` following
   [v1 cache manifest](v1-cache-manifest.md). The manifest records artifact,
   index, log, lock, and temp roots.

10. Remove bincode acceptance only after the rollout policy records a completed
    broker-default window.

## Runtime Selection

zccache reads `RUNNING_PROCESS_DISABLE` before daemon discovery:

| Value | zccache behavior during transition |
|---|---|
| unset | use the release default recorded in zccache rollout metadata |
| `1` | use `bincode-legacy` and the direct daemon endpoint |

The zccache diagnostics command prints the selected wire mode, protocol number,
broker instance, and daemon endpoint.

## Platform Behavior

zccache uses the broker endpoint returned by running-process and does not
derive pipe names locally.

| Platform | Broker endpoint behavior |
|---|---|
| Linux | Unix-domain socket under `$XDG_RUNTIME_DIR/running-process/broker`, with `/tmp/running-process-{uid}/broker` fallback |
| macOS | Unix-domain socket under `$TMPDIR/.rp-{uid}` with a 16-character hashed leaf |
| Windows | named pipe under `\\.\pipe\` |

zccache cache manifests live in the v1 platform registry:

| Platform | zccache manifest registry |
|---|---|
| Linux | `$XDG_DATA_HOME/running-process/manifests` |
| macOS | `~/Library/Application Support/running-process/manifests` |
| Windows | `%APPDATA%\running-process\manifests` |

## Test Matrix

- Golden-message tests for bincode and prost parity.
- Protocol rejection tests for stale, future, and malformed protocol numbers.
- Broker handshake tests for accepted, version-refused, and service-unknown
  replies.
- Cache manifest tests for artifact, index, lock, log, and temp roots.
- Per-platform endpoint tests for Linux, macOS, and Windows.
- Rollback tests with `RUNNING_PROCESS_DISABLE=1`.

## Release Checklist

- Bump `PROTOCOL_VERSION` in the prost transition release.
- Publish release notes naming the last bincode protocol number.
- Keep the direct bincode path in CI for the full transition window.
- Keep broker prost tests in CI for Linux, macOS, and Windows.
- Record the final bincode-removal release in zccache rollout metadata.
