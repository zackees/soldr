# v1 Admin Verbs

Admin verbs use the broker control pipe with `Frame.payload_protocol = 0xAD01`.
Every JSON response uses `schema_version: 1`.

Admin request frames carry `AdminRequest` protobuf payloads and response frames
carry `AdminReply` protobuf payloads. The Phase 4 control-socket accept loop
routes live Hello and admin frames over the same broker endpoint; focused tests
still use one-shot local-socket server/client helpers.

## Frame Payload

```protobuf
message AdminRequest {
  AdminVerb verb = 1;
  bool json = 2;
  string service_name = 3;
  string output_path = 4;
}

message AdminReply {
  AdminReplyKind kind = 1;
  string body = 2;
  uint32 exit_code = 3;
  string content_type = 4;
}
```

## Common Envelope

```json
{
  "schema_version": 1,
  "command": "status",
  "generated_at_unix_ms": 1700000000000
}
```

Additional command-specific fields live beside the common envelope fields.

## `status --json`

Returns broker liveness and backend summary.

```json
{
  "schema_version": 1,
  "command": "status",
  "generated_at_unix_ms": 1700000000000,
  "broker_instance": "shared",
  "broker_pid": 1234,
  "uptime_seconds": 12.5,
  "accepting_hello": true,
  "connections_open": 1,
  "backends": [
    {
      "service_name": "zccache",
      "service_version": "1.11.20",
      "pid": 4321,
      "backend_pipe": "rpb-v1-deadbeefdeadbeef-be-abababababababababababababababab",
      "last_active_unix_ms": 1700000000000,
      "state": "running"
    }
  ]
}
```

## `dump --json`

Returns a full debug snapshot.

```json
{
  "schema_version": 1,
  "command": "dump",
  "generated_at_unix_ms": 1700000000000,
  "broker_instance": "shared",
  "effective_config": {
    "broker": {
      "broker_instance": {
        "value": "shared",
        "source": "runtime"
      },
      "broker_pid": {
        "value": 1234,
        "source": "runtime"
      },
      "accepting_hello": {
        "value": true,
        "source": "runtime"
      }
    },
    "protocol": {
      "admin_payload_protocol": {
        "value": "0xAD01",
        "source": "protocol-v1"
      },
      "envelope_version": {
        "value": 1,
        "source": "protocol-v1"
      },
      "framing_version": {
        "value": 1,
        "source": "protocol-v1"
      }
    },
    "limits": {
      "max_frame_bytes": {
        "value": 16777216,
        "source": "protocol-v1"
      },
      "max_hello_bytes": {
        "value": 65536,
        "source": "protocol-v1"
      },
      "connections_open": {
        "value": 1,
        "source": "runtime"
      }
    },
    "spawn_budget": {
      "default_attempts_per_window": {
        "value": 3,
        "source": "default"
      },
      "default_window_ms": {
        "value": 30000,
        "source": "default"
      },
      "active_budget_rows": {
        "value": 1,
        "source": "runtime"
      }
    },
    "diagnostics": {
      "bundle_format": {
        "value": "tar.gz",
        "source": "schema-v1"
      },
      "bundle_mode": {
        "value": "metadata-only",
        "source": "schema-v1"
      },
      "redactions": {
        "value": [
          "home",
          "secret-env",
          "acl-identities"
        ],
        "source": "schema-v1"
      }
    }
  },
  "backend_table": [],
  "spawn_budgets": [
    {
      "broker_instance": "shared",
      "service_name": "zccache",
      "service_version": "1.11.20",
      "attempts_used": 1,
      "remaining": 2,
      "in_flight": false,
      "retry_after_ms": null
    }
  ],
  "recent_lifecycle_events": []
}
```

## `list-instances --json`

Returns every broker instance visible to the current user.

```json
{
  "schema_version": 1,
  "command": "list-instances",
  "generated_at_unix_ms": 1700000000000,
  "instances": [
    {
      "broker_instance": "shared",
      "pipe": "rpb-v1-deadbeefdeadbeef-shared",
      "pid": 1234,
      "state": "running"
    }
  ]
}
```

## `healthz`

Returns success when the broker process is alive and can answer admin frames.
The non-JSON response body is exactly:

```text
ok
```

## `readyz`

Returns success when the broker accepts new `Hello` requests. The non-JSON
response body is exactly:

```text
ready
```

During shutdown drain, `healthz` stays healthy and `readyz` returns failure.

## `backend-health <service> --json`

Returns backend health for one service.

```json
{
  "schema_version": 1,
  "command": "backend-health",
  "generated_at_unix_ms": 1700000000000,
  "service_name": "zccache",
  "backends": [
    {
      "service_version": "1.11.20",
      "pid": 4321,
      "state": "running",
      "last_hello_unix_ms": 1700000000000,
      "last_error": null
    }
  ]
}
```

## `config --effective --json`

Returns effective broker configuration and the source of each value.

```json
{
  "schema_version": 1,
  "command": "config",
  "generated_at_unix_ms": 1700000000000,
  "values": {
    "broker": {
      "broker_instance": {
        "value": "shared",
        "source": "runtime"
      }
    },
    "protocol": {
      "admin_payload_protocol": {
        "value": "0xAD01",
        "source": "protocol-v1"
      }
    },
    "limits": {
      "max_hello_bytes": {
        "value": 65536,
        "source": "protocol-v1"
      }
    },
    "spawn_budget": {
      "default_attempts_per_window": {
        "value": 3,
        "source": "default"
      }
    },
    "diagnostics": {
      "bundle_mode": {
        "value": "metadata-only",
        "source": "schema-v1"
      }
    }
  }
}
```

## `diagnose --output bundle.tar.gz`

Returns deterministic diagnostic bundle metadata. The Phase 4 renderer does
not create the archive; later lifecycle work can consume this schema to write
the tarball.

```json
{
  "schema_version": 1,
  "command": "diagnose",
  "generated_at_unix_ms": 1700000000000,
  "output": "bundle.tar.gz",
  "bundle": {
    "format": "tar.gz",
    "mode": "metadata-only",
    "created": false,
    "entries": [
      {
        "path": "admin/status.json",
        "kind": "json",
        "source": "status",
        "required": true,
        "redacted": false
      },
      {
        "path": "admin/dump.json",
        "kind": "json",
        "source": "dump",
        "required": true,
        "redacted": true
      },
      {
        "path": "config/effective.json",
        "kind": "json",
        "source": "effective-config",
        "required": true,
        "redacted": false
      },
      {
        "path": "metrics/openmetrics.txt",
        "kind": "openmetrics",
        "source": "metrics",
        "required": true,
        "redacted": false
      },
      {
        "path": "events/lifecycle.jsonl",
        "kind": "jsonl",
        "source": "lifecycle-events",
        "required": false,
        "redacted": true
      },
      {
        "path": "manifest/backend-manifests.json",
        "kind": "json",
        "source": "backend-manifest-index",
        "required": false,
        "redacted": true
      },
      {
        "path": "process/backends.json",
        "kind": "json",
        "source": "backend-table",
        "required": true,
        "redacted": true,
        "record_count": 1
      },
      {
        "path": "system/summary.json",
        "kind": "json",
        "source": "host-summary",
        "required": false,
        "redacted": true
      }
    ]
  },
  "files": [
    "admin/status.json",
    "admin/dump.json",
    "config/effective.json",
    "metrics/openmetrics.txt",
    "events/lifecycle.jsonl",
    "manifest/backend-manifests.json",
    "process/backends.json",
    "system/summary.json"
  ],
  "redactions": [
    "home",
    "secret-env",
    "acl-identities"
  ],
  "redaction_policy": [
    {
      "name": "home",
      "replacement": "~"
    },
    {
      "name": "secret-env",
      "matches": [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASS"
      ]
    },
    {
      "name": "acl-identities",
      "replacement": "stable-hash"
    }
  ]
}
```

## `metrics`

Returns OpenMetrics text. Metric names are defined in
[v1 observability](v1-observability.md).

## `doctor [--json] [--service-def-dir <dir>]`

Read-only local environment diagnostics (#354, v1.x-5 from #228). Unlike the
admin verbs above, `doctor` runs entirely in the CLI process — it does not
require (or speak) the admin payload protocol, so it works when no broker is
running at all. `--socket <endpoint>` overrides the probed broker endpoint;
otherwise the per-user shared-broker endpoint is derived.

Checks (each PASS / WARN / FAIL with a one-line detail):

- every `RUNNING_PROCESS_*` environment knob, with a loud WARN when the
  test-only `RUNNING_PROCESS_FAKE_BACKEND` seam is set
- broker endpoint reachability, including a deadline-bounded Hello probe
  that reports daemon version, protocol range, and decoded capability bits
- service-definition directory permissions plus per-`.servicedef` file
  parse/validation results
- stale `*.sock` files in the broker runtime directory (Unix; reported,
  never deleted — doctor is read-only)
- derived pipe/socket path length against the platform budget
  (`MAX_PATH` / `sun_path`)
- systemd KillMode (#391): WARN when the owning unit uses
  `KillMode=control-group` (or it cannot be determined) — Linux only,
  silent when not systemd-managed
- crate, protocol, and framing versions

Exit code is `0` when no check FAILs (WARNs do not fail) and `1` otherwise.

```json
{
  "schema_version": 1,
  "command": "doctor",
  "exit_code": 0,
  "checks": [
    {
      "check": "env:RUNNING_PROCESS_DISABLE",
      "status": "PASS",
      "detail": "unset (broker enabled)"
    }
  ]
}
```
