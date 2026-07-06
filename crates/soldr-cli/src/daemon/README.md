# daemon

The `soldr-daemon` long-lived process and its IPC surface.

- `protocol.rs` — the domain `Request` / `Response` enums, `StatusInfo`,
  `CompileStatsInfo`, and `PROTOCOL_VERSION` (bumped on any wire change).
- `wire.proto` — protobuf schema (reference).
- `wire.rs` — the prost-backed encode/decode between the domain types and the
  length-prefixed IPC frames. The prost message definitions live in
  `wire_proto.rs` and the round-trip tests in `wire_tests.rs` (split out to
  stay under the LOC guard; both wired in via `#[path]`).
- `client.rs` — client helpers used by the CLI/wrapper to talk to the daemon
  (`status`, `flush_caches`, `compile_stats`, `build_session_*`, …).
- `server.rs` — the accept loop + per-request handlers, including the
  embedded zccache `Compile` / `FlushCaches` / `CompileStats` verbs.
- `zccache_link.rs` / `db.rs` / `lifecycle.rs` — linked-zccache state,
  redb-backed build/target registry, and daemon spawn/adoption.

The daemon owns the in-process embedded zccache service; wrappers dispatch
compiles to it over the `Request::Compile` IPC verb (soldr#977/#980/#1081),
and `soldr session end` reads cumulative compile stats via
`Request::CompileStats` (soldr#1368).
