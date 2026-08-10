# v1 Wire Envelope

The authoritative schema is:

```text
crates/running-process/proto/broker_v1/broker_v1_envelope.proto
```

Every broker control-plane connection uses v1 framing followed by a prost
message.

## Frame Layout

```text
[u8 framing_version = 1][u32 little_endian_body_length][prost body]
```

Committed limits:

| Limit | Value |
|---|---|
| Max frame body | 16 MiB |
| Max initial `Hello` body | 64 KiB |
| Framing version | `1` |
| Body encoding | prost |

## Message Roles

| Message | Role |
|---|---|
| `Frame` | Common envelope for broker requests, responses, events, and cancellation. |
| `Hello` | Required first control-plane request from a client. |
| `HelloReply` | Broker response to `Hello`; contains `Negotiated` or `Refused`. |
| `Negotiated` | Successful backend selection and connection instructions. |
| `Refused` | Stable refusal reason and retry metadata. |

## Frame Fields

| Field | Rationale |
|---|---|
| `envelope_version` | Carries the v1 logical envelope version inside the prost body. |
| `kind` | Distinguishes requests, responses, events, and cancellation frames. |
| `payload_protocol` | Separates core control-plane payloads from admin verb payloads. |
| `payload` | Keeps service/admin payload bytes opaque to generic frame handling. |
| `request_id` | Correlates request and response frames. |
| `payload_encoding` | Reserves compression choices without changing the envelope. |
| `deadline_unix_ms` | Gives the receiver an absolute deadline for bounded work. |
| `traceparent` | Carries W3C trace context. |
| `tracestate` | Carries vendor-specific trace context. |
| `reserved 10 to 15` | Keeps low-numbered expansion slots unavailable for accidental reuse. |

## Hello Fields

| Field | Rationale |
|---|---|
| `client_min_protocol` | Lowest protocol the client accepts. |
| `client_max_protocol` | Highest protocol the client accepts. |
| `service_name` | Canonical service selector, `[a-z0-9-]{1,64}`. |
| `wanted_version` | Semver backend version requested by the client. |
| `client_version` | Informational client version for diagnostics. |
| `client_capabilities` | Additive feature bitmap. |
| `auth_token` | Reserved capability-delegation field. |
| `request_id` | Human-readable correlation id for the initial handshake. |
| `connection_id` | Zero on request; broker-assigned id on reply echo. |
| `peer_pid` | Telemetry only; broker verifies peer identity through OS APIs. |
| `client_lib_name` | Identifies the library that opened the connection. |
| `client_lib_version` | Identifies the library version. |
| `peer_attestation_nonce` | Reserved challenge-response field. |
| `capability_token` | Reserved capability-delegation field. |
| `client_keepalive_secs` | Client's requested idle keepalive. |
| `reserved 16 to 20` | Future additive fields are planned, not improvised. |

## Negotiated Fields

| Field | Rationale |
|---|---|
| `negotiated_protocol` | Protocol version selected by broker and client. |
| `daemon_version` | Broker binary version for diagnostics. |
| `backend_pipe` | Direct backend pipe or socket for the client to connect to. |
| `warnings` | Non-fatal notices for logs and migration support. |
| `server_capabilities` | Additive broker feature bitmap. |
| `keepalive_interval_secs` | Broker-selected keepalive interval. |
| `handle_passed_token` | Present when handoff optimization delivered an already-open handle. |
| `connection_id` | Broker-assigned connection id for tracing and logs. |

## Refused Fields

| Field | Rationale |
|---|---|
| `reason` | Human-readable explanation for operators. |
| `daemon_min_protocol` | Lowest broker protocol accepted. |
| `daemon_max_protocol` | Highest broker protocol accepted. |
| `code` | Stable machine-readable refusal code. |
| `details` | Structured key-value details. |
| `retry_after_ms` | Backoff hint for retryable refusals. |

## Conceptual Hello Example

The wire format is binary prost. This JSON shape documents the logical values:

```json
{
  "client_min_protocol": 1,
  "client_max_protocol": 1,
  "service_name": "zccache",
  "wanted_version": "1.11.20",
  "client_version": "zccache-cli/0.5.0",
  "client_capabilities": 1,
  "request_id": "hello-1700000000000",
  "connection_id": 0,
  "peer_pid": 4242,
  "client_lib_name": "running-process",
  "client_lib_version": "4.0.3",
  "client_keepalive_secs": 60
}
```

## Conceptual Negotiated Example

```json
{
  "negotiated_protocol": 1,
  "daemon_version": "running-process-broker-v1/4.0.3",
  "backend_pipe": "\\\\.\\pipe\\rpb-v1-deadbeefdeadbeef-be-abababababababababababababababab",
  "warnings": [],
  "server_capabilities": 1,
  "keepalive_interval_secs": 60,
  "connection_id": 99
}
```

## Refusal Codes

| Code | Meaning |
|---|---|
| `ERROR_VERSION_UNSUPPORTED` | Client and broker protocol ranges do not overlap. |
| `ERROR_SERVICE_UNKNOWN` | No valid service definition exists for the service. |
| `ERROR_BACKEND_SPAWN_FAILED` | Backend startup failed inside the bounded spawn policy. |
| `ERROR_RATE_LIMITED` | Spawn or request budget is exhausted. |
| `ERROR_SHUTTING_DOWN` | Broker is draining and refuses new work. |
| `ERROR_PEER_REJECTED` | Peer credentials or ACL checks failed. |
| `ERROR_INTERNAL` | Broker hit an internal invariant failure. |
| `ERROR_VERSION_BLOCKED` | Requested service version is below policy floor. |
| `ERROR_FD_PRESSURE` | Reserved v1.x resource-pressure refusal. |
