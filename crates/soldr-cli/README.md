# soldr-cli

The soldr command-line interface and its sibling binaries.

## Binaries

- `soldr` (`src/main.rs`) — the primary CLI: cargo front door, tool
  fetch/dispatch, cache/session commands, doctor, archive, shims.
- `soldr-daemon` (`src/bin/soldr_daemon.rs`) — long-lived daemon that owns
  target tracking, build-session correlation, and the in-process embedded
  zccache compile service.
- `soldr-shim` (`src/bin/soldr_shim.rs`) — multi-tool argv[0] shim installed
  under each toolchain tool name.
- `soldr-clang-shim` (`src/bin/soldr_clang_shim.rs`) — `clang`/`clang++`
  wrapper that routes to `clang-cl` for MSVC targets.
- `zccache-soldr` (`src/bin/zccache_soldr.rs`) — dedicated `RUSTC_WRAPPER`
  shim that forwards rustc invocations to the daemon's embedded zccache
  service over IPC.
- `zccache` (`src/bin/zccache_embedded.rs`) — compiled-in zccache CLI
  trampoline (`zccache::cli::commands::run()`) from the in-tree
  `_vender/zccache` library dep. `soldr zccache <args>` execs it, so no
  external managed zccache binary is ever downloaded (soldr#1368).

## Library

`src/lib.rs` re-exports the modules consumed by the integration tests under
`tests/`. Production code goes through the `[[bin]]` targets.
