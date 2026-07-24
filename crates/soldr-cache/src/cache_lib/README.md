# cache_lib

Cache-management subsystem shared between the `soldr` CLI and the
soldr-daemon. Owns the on-disk layout under `$SOLDR_CACHE_DIR`, the
session-state DB, garbage-collection passes, and the per-build `target/`
prune that ships to zccache via the thin Rust artifact plan.

## Modules

- `mod.rs` — public re-exports + the shared cache-root path helpers.
- `state_db.rs` — `redb`-backed registry of tracked `target/` dirs and
  cargo registry-src caches.
- `auto_gc.rs` — background cache-GC trigger.
- `cargo_global_cache.rs` — `cargo`-native `clean gc` invocation
  helpers (`$CARGO_HOME` cleanup orchestration).
- `gc.rs` — soldr-side GC pass orchestrator: locations, summary, purge.
- `save.rs` — `soldr save` / `soldr load` archive plumbing (mtime-
  preserving tar.zst bundle of the build cache + protobuf
  source/cache-file manifest, including base+delta cache layers).
- `prune_target.rs` — explicit `target/` maintenance (orphan hash-sibling
  removal + `--keep-latest` aggressive mode). Tests live in
  `prune_target_tests.rs`, included via `#[path]` so `prune_target.rs`
  stays under the loc_guard 1K LOC budget.

## Conventions

- The cache root is `$SOLDR_CACHE_DIR` (or `~/.soldr/` when unset). All
  sub-paths derive from `SoldrPaths` in `crate::core`.
- Long source files split into `<name>.rs` + `<name>_tests.rs` and
  include the tests via `#[cfg(test)] #[path = "<name>_tests.rs"] mod
  tests;` to keep each file under the 1K LOC warn threshold.
