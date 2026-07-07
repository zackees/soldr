# soldr-cli

The soldr command-line interface and its remaining sibling binaries.

## Binaries

- `soldr` (`src/main.rs`) — the primary CLI: cargo front door, tool
  fetch/dispatch, cache/session commands, doctor, archive, shims.
- `soldr-daemon` (`src/bin/soldr_daemon.rs`) — long-lived daemon that owns
  target tracking, build-session correlation, and the in-process embedded
  zccache compile service.
- `zccache` (`src/bin/zccache_embedded.rs`) — compiled-in zccache CLI
  trampoline (`zccache::cli::commands::run()`) from the in-tree
  `_vender/zccache` library dep. `soldr zccache <args>` execs it, so no
  external managed zccache binary is ever downloaded (soldr#1368).

Toolchain shims (`cargo`, `rustc`, `rustfmt`, `clippy-driver`, `rustdoc`),
clang shims (`clang`, `clang++`), and the `zccache-soldr` wrapper are
multicall names for the main `soldr` binary. Installers create them as
hardlinks/copies of `soldr`; the release archive does not ship separate
shim executables.

## Library

`src/lib.rs` re-exports the modules consumed by the integration tests under
`tests/`. Production code goes through the `[[bin]]` targets.
