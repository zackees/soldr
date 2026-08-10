# v1 Frozen Commitments

This document lists the v1 broker commitments that stay stable for the lifetime
of `running-process-broker-v1`.

## Meaning of Frozen

Frozen means a breaking change requires a new broker major version:

- new binary name, such as `running-process-broker-v2`
- new proto package, such as `running_process.broker.v2`
- new pipe-name prefix
- independent v2 tests and documentation

The v1 binary and v1 schema remain buildable. Additive fields use reserved
field ranges. Incompatible behavior does not reuse v1 names.

## Framing Byte

Every broker control-plane frame starts with this five-byte header:

```text
[u8 framing_version = 1][u32 little_endian_body_length][prost body]
```

The framing byte is the bootstrap invariant. It lets a v1 broker identify the
wire version before decoding the prost body.

Committed values:

- framing byte: `1`
- length header: unsigned 32-bit little-endian integer
- max frame size: 16 MiB
- max `Hello` frame size: 64 KiB
- prost recursion limit: default prost limit

## Proto Package and Files

The v1 proto package is:

```proto
package running_process.broker.v1;
```

The authoritative proto files are:

- `crates/running-process/proto/broker_v1/broker_v1_envelope.proto`
- `crates/running-process/proto/broker_v1/broker_v1_manifest.proto`
- `crates/running-process/proto/broker_v1/broker_v1_service_def.proto`

Field numbers and reserved ranges are part of the v1 contract. New fields use
unclaimed field numbers or documented reserved expansion ranges. Removed field
numbers stay reserved.

## Pipe Names

The v1 pipe-name prefix is `rpb-v1`. The four canonical pipe classes are:

- shared broker
- private service broker
- explicit named broker instance
- unguessable backend pipe

See [v1 pipe naming](v1-pipe-naming.md) for platform paths and examples.

## Peer Identity

Broker pipe names include a 16-character lowercase hex user identity hash.
The hash input is platform-specific:

| Platform | Hash input |
|---|---|
| Windows | Current process token user SID bytes |
| Linux | UID plus machine id |
| macOS | UID plus platform UUID |

The literal SID, machine id, and platform UUID do not appear in pipe paths.

## Admin JSON Envelopes

Every admin verb that returns JSON uses a stable top-level envelope:

```json
{
  "schema_version": 1,
  "command": "status",
  "generated_at_unix_ms": 1700000000000
}
```

Field additions are allowed when old clients ignore unknown fields. Field
renames, type changes, and meaning changes require v2.

## Security Boundary

The v1 trust boundary is local IPC plus operating-system permissions. The v1
broker never exposes a TCP listener. TLS and network encryption are outside the
v1 contract because there is no network transport.

## Escape Hatch

`RUNNING_PROCESS_DISABLE=1` disables broker usage for participating
consumers. The direct daemon path remains available while v1 is in rollout.
