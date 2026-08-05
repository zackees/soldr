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
- `src/daemon/db.rs` — synchronous `state.redb` access for the daemon-owned
  tables; `db_async.rs` is the `spawn_blocking` wrapper the async handlers
  must use.
- `src/daemon/maintenance.rs` — the scheduled pressure (5 min) and full
  (24 h) cache-maintenance passes.
- `src/zccache_embedded/` — the embedded zccache service and its disk policy.

## State-database discipline

`~/.soldr/state.redb` is a single file shared by four process classes — the
`soldr cargo` front door, the per-compile rustc wrapper, this daemon, and the
reporting CLI — and redb takes an **exclusive whole-file lock per `Database`
handle**. `TargetRegistry::open` holds that lock *and* the process-wide
`state_db_open_lock` for the handle's entire lifetime (#608), so handle
lifetime is a cross-process concurrency decision, not a local one.

Three rules follow, each with a regression behind it:

1. **Never hold a handle across unbounded work.** Directory sizing, recursive
   deletion, and anything that waits on a human all outlast every other
   opener's budget (5 s `Required`, 50 ms `BestEffort`). The sanctioned shape
   is three phases — snapshot with the handle open, do the filesystem work
   with **no** handle open, then reopen for the bounded bookkeeping write.
   See `sweep_workspace_targets` in `src/daemon/maintenance.rs` and
   `cache_lib::gc::scan_released`. Fixed CLI-side in #1681, daemon-side in
   #2225 (reported as #2223, diagnosed in #2224).
2. **Acquire once per logical operation.** Opening per call turns one session
   start into several acquire/release cycles, each able to lose its record to
   a contended budget. Prefer the handle-taking `_in` variants in `db.rs`.
3. **Never open on a tokio worker.** A contended open parks the runtime
   thread for up to 5 s; async callers go through `db_async` (#1669).

Contention is recorded durably to `~/.soldr/logs/redb-contention.jsonl`;
`budget-exhausted` entries there are the signal that one of these rules has
regressed.

## Testing the lock rules

Concurrency tests against `state.redb` **must spawn a real second process.**
`state_db_open_lock` is an in-process mutex, so a second opener on another
thread merely *waits* on it and then succeeds — it never surfaces redb's
`Database already open. Cannot acquire lock.`, and the test passes against
broken code. That is precisely how #2225 survived. See
`sweep_never_holds_state_db_across_filesystem_work` in
`src/daemon/maintenance.rs` for the established shape, and
`cache_lib::redb_lock`'s `subprocess_lock_holder` for the general idiom.
