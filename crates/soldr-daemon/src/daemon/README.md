# daemon

The `soldr-daemon` long-lived process and its IPC surface.

- `protocol.rs` — the domain `Request` / `Response` enums, `StatusInfo`,
  `CompileStatsInfo`, and `PROTOCOL_VERSION` (bumped on any wire change).
- `wire.rs` — the prost-backed encode/decode between the domain types and the
  length-prefixed IPC frames. The pure wire schema — prost message
  definitions (`wire_proto.rs`), the `wire.proto` reference schema, redb
  row-tag helpers, and `WireDecodeError` — lives in `crates/soldr-core/src/core/wire.rs`
  (#1490 Phase 0, so `cache_lib` needs no edge into `daemon`) and is
  re-exported here at the historical paths. Round-trip tests stay in
  `wire_tests.rs` (wired in via `#[path]`).
- `client.rs` — client helpers used by the CLI/wrapper to talk to the daemon
  (`status`, `flush_caches`, `compile_stats`, `build_session_*`, …).
- `server.rs` — the accept loop + per-request handlers, including the
  embedded zccache `Compile` / `FlushCaches` / `CompileStats` verbs.
- `db.rs` / `lifecycle.rs` — redb-backed build/target registry and daemon
  spawn/adoption.
- `compile_delivery.rs` — always-on JSONL record of compiles the daemon ran
  but could not hand back (soldr#1857). See below.

## Undelivered compiles (`compile-delivery.jsonl`)

A compile can succeed daemon-side and still surface to cargo as a bare
`exit 1` with no diagnostics, because the result is lost between the
compile future completing and the bytes reaching the wrapper's stdio.
Two things happen in that window and neither used to leave a trace: a
mid-compile client disconnect (recorded only into `compile_trace`, which
is inert unless `SOLDR_DAEMON_TRACE` is set) and a failure writing the
reply frames (a `tracing::warn!` on a detached daemon, i.e. nowhere).

Both now append a row to
`<cache>/soldr-daemon/logs/compile-delivery.jsonl`, always on, one event
per line, listed by `soldr logs paths` as `soldr-compile-delivery-log`:

```jsonl
{"schema_version":1,"ts_ms":…,"pid":…,"event":"client_disconnected","detail":"eof",…}
{"schema_version":1,"ts_ms":…,"pid":…,"event":"reply_write_failed","detail":"done:BrokenPipe","exit_code":0,…}
```

A row with `"exit_code": 0` is the #1857 signature outright: a compile
that succeeded and was never delivered. `client_disconnected` rows
answer the companion question — whether wrapper processes are dying
mid-compile — with `detail` separating a wrapper that exited (`eof`)
from a pipe the OS tore down (`read_error:…`) from a protocol violation
(`unexpected_bytes:…`).

Related invariant, enforced by
`completed_compile_is_not_discarded_by_a_simultaneous_disconnect`: the
`biased` `select!` in `race_against_disconnect` must never discard a
compile that has already finished. It polls the reader first on every
tick, so before #1857 a compile completing in the same tick as the
disconnect signal was thrown away after zccache had already journaled
`exit_code: 0`.

The daemon owns the in-process embedded zccache service; wrappers dispatch
compiles to it over the `Request::Compile` IPC verb (soldr#977/#980/#1081),
and `soldr session end` reads cumulative compile stats via
`Request::CompileStats` (soldr#1368).
