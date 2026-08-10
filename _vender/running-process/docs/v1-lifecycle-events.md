# v1 Lifecycle Events

The authoritative schema is part of:

```text
crates/running-process/proto/broker_v1/broker_v1_manifest.proto
```

`LifecycleEvent` records broker and backend lifecycle activity in a compact,
append-only log.

## Size Contract

Each encoded event stays at or below 512 bytes. This is the portable POSIX
`PIPE_BUF` floor and keeps appends atomic across supported platforms.

## Event Fields

| Field | Rationale |
|---|---|
| `ts_ms` | Event timestamp in Unix milliseconds. |
| `pid` | Process that emitted or owns the event. |
| `service_name` | Canonical service name. |
| `kind` | Stable event kind. |
| `reason` | Human-readable reason. |
| `extra` | Small structured details. |
| `severity_number` | OpenTelemetry severity number, 1 through 24. |
| `severity_text` | OpenTelemetry severity label. |
| `request_id` | Correlates events with handshakes and admin verbs. |
| `connection_id` | Broker-assigned connection id. |
| `broker_instance` | Shared, private, or explicit broker instance. |

## Event Kinds

| Event | Meaning |
|---|---|
| `SPAWN_ATTEMPT` | Broker started backend spawn coordination. |
| `SPAWN` | Backend process started. |
| `DIED_IDLE` | Backend exited after idle policy. |
| `DIED_SHUTDOWN` | Backend exited during broker shutdown. |
| `DIED_CRASH` | Backend exited unexpectedly. |
| `VERSION_MISMATCH` | Requested version did not match policy or backend state. |
| `REPLACED_BY_NEWER` | Backend was replaced by a newer allowed version. |
| `HELLO_ACCEPTED` | Broker accepted a client handshake. |
| `HELLO_REFUSED` | Broker refused a client handshake. |
| `SERVICE_DEF_LOADED` | Service definition loaded successfully. |
| `SERVICE_DEF_CHANGED` | Service definition changed after previous load. |
| `BROADCAST_SENT` | Broker sent a broadcast control operation. |
| `BROADCAST_ACK` | Backend acknowledged a broadcast. |
| `BROADCAST_TIMEOUT` | Backend missed the broadcast deadline. |
| `PROTOCOL_DOWNGRADE` | Negotiation selected a lower mutually supported protocol. |
| `CACHE_CORRUPTION_DETECTED` | Manifest or cache integrity check failed. |
| `RESOURCE_PRESSURE` | Broker observed resource pressure. |
| `SECURITY_VIOLATION` | ACL, peer credential, or path hardening check failed. |
| `TEARDOWN_HOOK_FAILED` | A root teardown hook failed. |
| `MANIFEST_REWRITTEN` | Manifest was rewritten after validation or migration. |

## Reserved Ranges

| Range | Use |
|---|---|
| `21 to 30` | v1.x resource-pressure events such as FD and inode pressure. |
| `31 to 40` | v1.x cleanup and doctor events. |
| `41 to 50` | Long-range future expansion. |

Reserved numbers stay unavailable until their documented expansion area is
implemented.

## Append Semantics

| Platform | Append rule |
|---|---|
| Linux and macOS | Open with `O_APPEND`; write encoded event in one call. |
| Windows | Open with `FILE_APPEND_DATA`; write encoded event in one call. |

Log rotation is broker-owned. External rename-based rotation is not part of
the v1 contract because it races with append semantics.
