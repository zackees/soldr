# soldr-daemon

The long-lived soldr daemon runtime: lifecycle and root ownership, the IPC
server, v2 broker adoption, and the in-process embedded zccache compile
service.

## Layout

- `src/daemon/lifecycle.rs` — root ownership, endpoint probing, self-shutdown
  when the daemon's own image disappears (#1987).
- `src/daemon/server.rs` — the IPC request handlers.
- `src/daemon/build_session_ops.rs` — the three state-DB-backed session
  handlers, split out of `server.rs` for the per-file LOC ratchet.
- `src/daemon/client.rs` — the client side used by the CLI, including the
  best-effort `RecordTargetTouch` write on every rustc-wrapper call.
- `src/daemon/db.rs` — synchronous `state.sqlite3` access for the daemon-owned
  tables; `db_async.rs` is the `spawn_blocking` wrapper the async handlers
  must use.
- `src/daemon/maintenance.rs` — the scheduled pressure (5 min) and full
  (24 h) cache-maintenance passes.
- `src/zccache_embedded/` — the embedded zccache service and its disk policy.

## State-database discipline

`~/.soldr/state.sqlite3` is shared by the Cargo front door, compiler wrappers,
the daemon, and reporting commands. SQLite WAL permits concurrent readers and
one writer. Connections do not hold an exclusive file lock or a process-wide
open mutex for their lifetime. Write transactions use `BEGIN IMMEDIATE` and
wait up to 5 seconds for writer contention; latency-critical wrapper bookkeeping uses a
50 ms budget. See `soldr-cache/src/cache_lib/state_store.rs`.

1. **Keep transactions short.** Filesystem sizing, deletion, subprocesses,
   and human prompts belong outside a database transaction. GC snapshots rows,
   releases its connection, performs filesystem work, then records outcomes.
   This preserves the phases introduced in #1681/#2225 without relying on
   the removed redb exclusive-open behavior.
2. **Reuse a connection within one logical operation.** Prefer the `_in`
   variants when several database calls belong to one operation. A connection
   alone does not serialize other connections.
3. **Keep blocking database work off tokio workers.** SQLite opens, writes,
   and commits are synchronous; a contended writer can consume its full busy
   budget. Async database operations use the blocking pool through `db_async`; cook
   handlers run their cook-index operations with `spawn_blocking`.

## Testing transaction contention

Use independent connections and an explicitly held write transaction to test
writer contention. Merely holding a `TargetRegistry` connection is no longer a
negative control: another connection can read and write while it remains open.
Cross-process fixtures still exercise the boundary between maintenance and the
front door, but their successful second open proves reachability at the fixture
barrier, rather than proving that every connection was dropped. SQLite
transaction tests in `state_store.rs` cover the writer-lock behavior directly.
