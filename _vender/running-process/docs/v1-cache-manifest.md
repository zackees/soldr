# v1 Cache Manifest

The authoritative schema is:

```text
crates/running-process/proto/broker_v1/broker_v1_manifest.proto
```

`CacheManifest` records service-owned cache roots, runtime roots, daemon
identity, cleanup policy, and observability metadata. It is the data source for
the standalone cleanup tool and broker diagnostics.

## Storage Locations

Each manifest is stored in two places:

| Location | Purpose |
|---|---|
| `<cache_root>/.running-process-manifest.pb` | Local manifest beside the cache data. |
| `<broker_state>/manifests/{service}-{version}.pb` | Central registry for cross-service cleanup and diagnostics. |

The file format is prost-encoded protobuf bytes.

## Top-Level Field Groups

| Group | Fields | Rationale |
|---|---|---|
| OCI-style identity | `manifest_schema_version`, `media_type`, `self_sha256` | Stable content identity and self-checking. |
| Host identity | `host` | Prevents stale state from another boot, namespace, or host. |
| Operation state | `current_operation` | Shows long-running maintenance work. |
| TTL | `valid_until_unix_ms` | Bounds stale manifest reuse. |
| Service identity | `service_name`, `service_version`, `broker_envelope_version` | Selects service and broker contract. |
| Activity | `created_at_unix_ms`, `last_active_unix_ms` | Drives dormant cleanup. |
| Roots | `roots` | Declares cache, config, logs, locks, runtime, and temp roots. |
| Daemon | `current_daemon` | Identifies the live daemon for safety checks. |
| Cleanup | `cleanup_policy` | Defines retention policy. |
| Instance | `broker_instance` | Ties the manifest to a trust domain. |
| Dependencies | `depends_on`, `provides` | Supports ordered cleanup and diagnostics. |
| Observability | `observability` | Exposes metrics, logs, and health endpoints. |
| Bundle | `bundle_id` | Groups related roots for diagnostics and cleanup. |

## Root Kinds

| Kind | Cleanup rule |
|---|---|
| `CACHE_DATA` | Pruned according to cleanup policy. |
| `CACHE_LOGS` | Rotated and included in diagnostics. |
| `CACHE_LOCKS` | Removed only after liveness checks. |
| `CACHE_RUNTIME` | Removed when no live process references it. |
| `CACHE_TMP` | Cleared aggressively after liveness checks. |
| `CACHE_CONFIG` | Preserved. |
| `CACHE_INDEX` | Pruned by its own quota and teardown policy. |
| `CACHE_JOURNAL` | Preserved until storage-specific recovery is complete. |
| `CACHE_SECRETS` | Never logged and never pruned by generic cleanup. |

## Storage Dispositions

| Disposition | Meaning |
|---|---|
| `PRUNE_ON_UNINSTALL` | Delete during uninstall. |
| `PRESERVE_ACROSS_UNINSTALL` | Keep after uninstall. |
| `PRUNE_WHEN_DORMANT` | Delete when dormant policy matches. |
| `NEVER_PRUNE` | User-owned data. |
| `PRUNE_ON_CRASH` | Delete only while the writing process is known dead. |

## Example: zccache

```yaml
service_name: zccache
service_version: 1.11.20
broker_envelope_version: v1
broker_instance: shared
roots:
  - kind: CACHE_DATA
    path: ~/.cache/zccache/artifacts
    disposition: PRUNE_WHEN_DORMANT
    quota:
      hard_max_bytes: 10000000000
      soft_target_bytes: 5000000000
  - kind: CACHE_LOGS
    path: ~/.cache/zccache/logs
    disposition: PRUNE_ON_UNINSTALL
cleanup_policy:
  dormant_after_secs: 2592000
  keep_last_n_versions: 2
  keep_current: true
observability:
  metrics_endpoint: broker-admin:metrics
  log_path: ~/.cache/zccache/logs/lifecycle.pb
```

## Example: clud

```yaml
service_name: clud
service_version: 2.0.5
broker_envelope_version: v1
broker_instance: shared
roots:
  - kind: CACHE_RUNTIME
    path: ~/.cache/clud/running-process/runtime
    disposition: PRUNE_WHEN_DORMANT
  - kind: CACHE_LOCKS
    path: ~/.cache/clud/running-process/locks
    disposition: PRUNE_ON_CRASH
  - kind: CACHE_CONFIG
    path: ~/.config/clud
    disposition: NEVER_PRUNE
cleanup_policy:
  dormant_after_secs: 1209600
  keep_last_n_versions: 2
  keep_current: true
```

## Example: soldr

```yaml
service_name: soldr-daemon
service_version: 0.8.0
broker_envelope_version: v1
broker_instance: shared
roots:
  - kind: CACHE_DATA
    path: ~/.cache/soldr/state.redb
    disposition: PRESERVE_ACROSS_UNINSTALL
    teardown_hook:
      kind: TEARDOWN_REDB_COMPACT
      timeout_secs: 30
  - kind: CACHE_RUNTIME
    path: ~/.cache/soldr/pinned-bin
    disposition: PRESERVE_ACROSS_UNINSTALL
cleanup_policy:
  dormant_after_secs: 2592000
  keep_last_n_versions: 2
  keep_current: true
depends_on:
  - service_name: running-process
    min_version: 4.0.3
    optional: false
```

## Integrity Rules

- `self_sha256` is computed over the serialized manifest with `self_sha256`
  zeroed.
- Manifests from a previous boot are stale.
- Temp files are created beside the target manifest before replacement.
- Cross-filesystem replacement is invalid.
- Lifecycle events encoded into logs stay at or below 512 bytes.
