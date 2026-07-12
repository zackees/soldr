# soldr-cli

The soldr command-line interface and its multicall aliases.

## Binaries

- `soldr` (`src/main.rs`) — the primary CLI: cargo front door, tool
  fetch/dispatch, cache/session commands, doctor, archive, shims.
- `soldr-daemon` (`src/daemon_entry.rs`) — long-lived daemon that owns
  target tracking, build-session correlation, and the in-process embedded
  zccache compile service.
- `soldr zccache <args>` dispatches the allowlisted zccache CLI surface
  in-process through `zccache::cli::commands::run_with_args()` from the
  in-tree `_vender/zccache` library dep; no zccache executable is shipped,
  materialized, or spawned (soldr#1593).

Toolchain shims (`cargo`, `rustc`, `rustfmt`, `clippy-driver`, `rustdoc`),
clang shims (`clang`, `clang++`), and the `zccache-soldr` wrapper are
multicall names for the main `soldr` binary. Installers create them as
hardlinks/copies of `soldr`; the release archive does not ship separate
shim executables. `soldr-daemon` uses the same mechanism; `soldr` is the
crate's only compiled `[[bin]]` target.

## Library

`src/lib.rs` re-exports the modules consumed by the integration tests under
`tests/`. Production code goes through the `soldr` `[[bin]]` target.
