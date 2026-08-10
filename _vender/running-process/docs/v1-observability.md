# v1 Observability

The v1 broker exposes structured logs, lifecycle events, admin JSON, metrics,
trace context propagation, and diagnostic bundles.

## Structured Logging

Production logs default to JSON. Text logs are for local development.

Environment:

```text
RP_BROKER_LOG_FORMAT=json
RP_BROKER_LOG_FORMAT=text
```

Log records include:

- timestamp
- level
- broker instance
- service name
- service version
- request id
- connection id
- event kind
- error code

## Lifecycle Events

`LifecycleEvent` is the durable event format. It uses OpenTelemetry
`severity_number` and `severity_text` fields. See
[v1 lifecycle events](v1-lifecycle-events.md).

## Trace Context

Every `Frame` carries:

- `traceparent`
- `tracestate`

The broker keeps the full `Frame` beside the decoded `Hello` during
negotiation. `traceparent`, `tracestate`, and `request_id` therefore survive
validation and are available to backend lifecycle work, metrics, and admin
diagnostics.

`HelloRequest::trace_context()` captures the backend-forwardable W3C headers
from the original frame.

## Metrics

Metrics use OpenMetrics text and the `running_process_broker_v1_` prefix.
The canonical names and label order live in `broker::server::metrics` and are
locked by `tests/broker/metrics_names_frozen.rs`.

| Metric | Type | Labels |
|---|---|---|
| `running_process_broker_v1_hello_total` | counter | `service`, `version`, `outcome` |
| `running_process_broker_v1_hello_duration_seconds` | histogram | `service` |
| `running_process_broker_v1_active_backends` | gauge | `service` |
| `running_process_broker_v1_spawn_attempts_total` | counter | `service`, `version`, `outcome` |
| `running_process_broker_v1_spawn_budget_remaining` | gauge | `service`, `version` |
| `running_process_broker_v1_connections_open` | gauge | none |
| `running_process_broker_v1_fd_usage_ratio` | gauge | none |
| `running_process_broker_v1_uptime_seconds` | gauge | none |

Metric renames require v2.

## Perf Guard

`broker::server::perf_guard` owns the frozen Hello latency budget:
P50 <= 200 us and P99 <= 1 ms over 10000 samples. The accept-loop benchmark
feeds real Hello round-trip samples into this module when the socket path lands.

## Diagnostic Bundle

`diagnose --output bundle.tar.gz` returns deterministic bundle metadata with
`mode: "metadata-only"` and `created: false` in Phase 4. The renderer does not
create the archive. Later lifecycle work can use the metadata to write a
tarball with:

- admin status JSON
- admin dump JSON
- effective broker config JSON
- OpenMetrics text
- decoded lifecycle events
- central backend manifest index
- running backend process ids
- OS and kernel summary

## Redaction

Diagnostic bundle metadata includes a redaction policy. Bundle writers must
redact:

- full home directory paths, rendered as `~`
- environment variables with `KEY`, `TOKEN`, `SECRET`, or `PASS` in the name
- ACL user and group identifiers, rendered as stable hashes

Redaction happens before writing any archive.
