# daemon

The `soldr-daemon` long-lived process and its IPC surface.

- `protocol.rs` — the domain `Request` / `Response` enums, `StatusInfo`,
  `CompileStatsInfo`, and `PROTOCOL_VERSION` (bumped on any wire change).
- `wire.rs` — the prost-backed encode/decode between the domain types and the
  length-prefixed IPC frames. The pure wire schema — prost message
  definitions (`wire_proto.rs`), the `wire.proto` reference schema, redb
  row-tag helpers, and `WireDecodeError` — lives in `src/core/wire.rs`
  (#1490 Phase 0, so `cache_lib` needs no edge into `daemon`) and is
  re-exported here at the historical paths. Round-trip tests stay in
  `wire_tests.rs` (wired in via `#[path]`).
- `client.rs` — client helpers used by the CLI/wrapper to talk to the daemon
  (`status`, `flush_caches`, `compile_stats`, `build_session_*`, …).
- `server.rs` — the accept loop + per-request handlers, including the
  embedded zccache `Compile` / `FlushCaches` / `CompileStats` verbs.
- `db.rs` / `lifecycle.rs` — redb-backed build/target registry and daemon
  spawn/adoption.

The daemon owns the in-process embedded zccache service; wrappers dispatch
compiles to it over the `Request::Compile` IPC verb (soldr#977/#980/#1081),
and `soldr session end` reads cumulative compile stats via
`Request::CompileStats` (soldr#1368).
