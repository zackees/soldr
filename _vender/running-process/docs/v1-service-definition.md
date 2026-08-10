# v1 Service Definition

The authoritative schema is:

```text
crates/running-process/proto/broker_v1/broker_v1_service_def.proto
```

`ServiceDefinition` tells the broker which backend binary serves a service,
which versions are allowed, and which broker isolation domain owns the service.

## Storage Directories

| Platform | Directory |
|---|---|
| Linux | `$XDG_CONFIG_HOME/running-process/services` |
| macOS | `~/Library/Application Support/running-process/services` |
| Windows | `%APPDATA%\running-process\services` |

The broker loads `.servicedef` files from the platform directory. The on-disk
format is prost-encoded protobuf bytes. The parent directory is current-user
only: mode `0700` on Unix and current-user-only ACL on Windows.

`ServiceDefinitionLoader` also honors `RUNNING_PROCESS_SERVICE_DEF_DIR` for
tests and development. A request for service `zccache` loads
`zccache.servicedef`; the decoded `service_name` must match the requested file
stem.

## Fields

| Field | Rationale |
|---|---|
| `service_name` | Canonical service name, `[a-z0-9-]{1,64}`. |
| `binary_path` | Canonical path to the backend binary. |
| `isolation` | Broker trust domain. |
| `explicit_instance` | Required when `isolation` is `EXPLICIT_INSTANCE`. |
| `per_version_binary_dir` | Canonical allow-list root for versioned backend binaries. |
| `min_version` | Semver floor for `wanted_version`. |
| `version_allow_list` | Optional strict set of allowed versions. |
| `labels` | Operator metadata used for diagnostics and policy. |

## Isolation Modes

| Mode | Behavior |
|---|---|
| `PRIVATE_BROKER` | Use a broker instance scoped to one service. |
| `SHARED_BROKER` | Use the per-user shared broker. |
| `EXPLICIT_INSTANCE` | Use the named broker instance in `explicit_instance`. |

## Private Broker Template

This textproto-style template is for authoring. Encode it as protobuf bytes
before writing the `.servicedef` file.

```textproto
service_name: "third-party-tool"
binary_path: "/opt/third-party-tool/bin/backend"
isolation: PRIVATE_BROKER
per_version_binary_dir: "/opt/third-party-tool/versions"
min_version: "1.0.0"
labels {
  key: "owner"
  value: "third-party-tool"
}
```

## Shared Broker Template

```textproto
service_name: "zccache"
binary_path: "/usr/local/bin/zccache"
isolation: SHARED_BROKER
per_version_binary_dir: "/usr/local/lib/zccache/versions"
min_version: "1.11.20"
version_allow_list: "1.11.20"
labels {
  key: "family"
  value: "first-party"
}
```

## Explicit Instance Template

```textproto
service_name: "zccache"
binary_path: "/usr/local/bin/zccache"
isolation: EXPLICIT_INSTANCE
explicit_instance: "ci-trusted"
per_version_binary_dir: "/opt/ci/zccache/versions"
min_version: "1.11.20"
labels {
  key: "trust-domain"
  value: "ci-trusted"
}
```

## Reload Rule

The broker validates service definitions on every `Hello` path through a lazy
reload check. The current loader implements this by re-reading the protobuf
file on every `lookup_or_reload` call. Invalid files are refused with a stable
machine-readable code and a human-readable reason.

## Version Policy

`min_version` is a semver floor. A `Hello.wanted_version` below that floor is
refused with `ERROR_VERSION_BLOCKED`.

`version_allow_list` is an optional exact-match allow-list. When it is
non-empty, the requested version must appear in the list. This prevents
resurrection of backend versions that were removed for correctness or security
reasons.
