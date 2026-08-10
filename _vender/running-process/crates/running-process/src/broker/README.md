# `broker`

The broker layer — backend identity, service-definition + cache-manifest
schemas, the v1 sync client + v2 async client, broker-owned HTTP
endpoint registry, and the broker-side service-def loader.

## Top-level layout

| file | what it owns |
|---|---|
| `mod.rs` | re-exports + module declarations |
| `adopt.rs` | v1 `AsyncBrokerSession::adopt` — async broker handoff API |
| `backend_handle.rs` | v1 `BackendHandle` / `DaemonProcess` — broker-side daemon identity probe |
| `backend_lib/` | broker-side backend lifecycle subsystems |
| `backend_lifecycle/` | broker-side process/identity bookkeeping |
| `backend_sdk/` | broker-facing SDK consumers compile against |
| `broker_http_port.rs` | env-driven HTTP port resolution (`BrokerHttpPort::resolve`) |
| `broker_http_server.rs` | placeholder HTTP server + aggregator iframe page |
| `brokered_backend.rs` | `BrokeredBackend` trait (#497) — fast-bind contract |
| `builders.rs` | v1 `ServiceDefinitionBuilder` + v1 `CacheManifestBuilder` |
| `capabilities.rs` | v1 Hello capability bitmap (FROZEN FOREVER per #228) |
| `client.rs` | v1 sync broker client (`connect_local_socket`, `BrokerClientError`, `RefusalKind`) |
| `client_v2.rs` | v2 sync broker client (`connect`, `connect_with_deadline`, `BrokerV2Error`) |
| `doctor.rs` | `running-process-doctor` health checks |
| `fs_health.rs` | broker-side filesystem health probes |
| `get_http_endpoint_dispatch.rs` | v2 `GetBrokerHttpEndpoint` RPC dispatch |
| `host_identity.rs` | `HostIdentity` derivation (machine_id, boot_id, fs_dev_id) |
| `http_endpoint_registry.rs` | broker-side `BackendId → port` registry |
| `lifecycle/` | pipe naming, SID derivation, identity errors |
| `manifest.rs` | v1 cache manifest schema + I/O (`CacheManifest`, `write_to_central`, `read_manifest`) |
| `protocol/` | v1 prost types (FROZEN FOREVER per #228) |
| `protocol_v2/` | v2 prost types + v2 builder/writer I/O helpers |
| `secure_dir.rs` | per-OS private-dir mode checks |
| `server/` | broker server — Hello router, service-def loader, accept loop |

## v1 ↔ v2 coexistence

v1 lives under `protocol`, `builders.rs::ServiceDefinitionBuilder` /
`builders.rs::CacheManifestBuilder`, and `server::service_def_loader`.
v2 lives under `protocol_v2/` (proto + builder + writer +
manifest I/O helpers). The two file formats coexist on disk by using
distinct extensions (`.servicedef` vs `.servicedef.v2`, `.pb` vs
`.v2.pb`); see `protocol_v2/README.md` for the file-name table.

v1 is FROZEN FOREVER (#228). All new schema work lands in v2.

## Cross-version helpers

A handful of helpers are intentionally version-agnostic and live at this
level rather than under `protocol/` or `protocol_v2/`:

- `lifecycle::names::validate_service_name` — same rules in both
  generations.
- `secure_dir::ensure_private_dir` — OS-level dir-mode check.
- `manifest::write_atomic` (`pub(super)`) — tempfile + sync + rename
  semantics shared by both v1's `write_manifest_file` and v2's
  `write_manifest_file_v2`.
- `manifest::central_registry_dir` — both generations write into the
  same directory; only the file extension distinguishes them.
- `manifest::ensure_central_registry_dir` / `central_manifest_path` —
  v2 reuses these for the registry dir + name validation (then re-stems
  to `.v2.pb`).
