# `broker::protocol_v2`

The v2 broker protocol surface. Houses the prost-generated types for the
`running_process.broker.v2` package plus the consumer-side I/O helpers
(`io.rs`, `manifest_io.rs`) that mirror the v1 builder/writer APIs in
[`super::builders`] and [`super::manifest`].

## File map

| file | what it owns |
|---|---|
| `mod.rs` | re-exports the prost-generated types from `OUT_DIR`; round-trip tests for `ServiceDefinition`, `BackendHttpReady`, `GetBrokerHttpEndpoint*` |
| `io.rs` | `ServiceDefinitionBuilder` + `write_service_definition_v2` + `service_definition_dir_v2` + `SERVICE_DEF_V2_EXTENSION` (slice 22b of [zackees/zccache#782]) |
| `manifest_io.rs` | `CacheManifestBuilder` + `write_to_root_v2` + `write_to_central_v2` + `ROOT_MANIFEST_FILE_V2` (slice 23-A of [zackees/zccache#782]) |

[zackees/zccache#782]: https://github.com/zackees/zccache/issues/782

## v1 ↔ v2 coexistence

Per the [broker-v2 design](https://github.com/zackees/running-process/issues/470),
v1 and v2 service-definition / cache-manifest files coexist in the same
on-disk directories. Distinct file extensions (`.servicedef` vs
`.servicedef.v2`, `.pb` vs `.v2.pb`) keep them from being misread by the
other generation's loader. v1 types are FROZEN FOREVER per #228; new
capability fields land in v2 instead.

## Field-number policy

When a v2 message ports a field from v1, the field number is reused for
human-readable-diff legibility (e.g. `ServiceDefinition.binary_path` is
field 2 in both). New v2-only fields start at 10 and grow upward.
