# cache

`soldr cache` / `soldr status` / `soldr session` command implementations.

- `mod.rs` — status/cache inspection, version, and cache-clearing commands;
  re-exports the submodule surface.
- `report.rs` — `soldr cache report` summary.
- `session.rs` — `soldr session start/end`, `soldr cache flush/shutdown`.
- `trim.rs` — `soldr cache prune-target` / `trim-target`.
- `release_worktree.rs` — release-worktree + trash-sweep helpers.

As of soldr#1368 these commands no longer resolve or spawn an externally
downloaded managed zccache binary. rustc compile caching lives in the
soldr-daemon's embedded zccache service; version/status here report the
compiled-in `zccache::core::VERSION`.
