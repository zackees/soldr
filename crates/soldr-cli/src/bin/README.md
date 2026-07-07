# soldr-cli binaries

Binary targets that ship in the `soldr-cli` crate alongside the main
`soldr` binary defined in `../main.rs`.

- **`soldr_daemon.rs`** — `soldr-daemon` long-lived helper process that
  owns target/ tracking (phase 1: `start`, `stop`, `status`).
- **`zccache_embedded.rs`** — `zccache` CLI trampoline backed by the
  in-tree zccache library.

The toolchain, clang, and `zccache-soldr` shim names are multicall names
for `soldr` itself and are installed as hardlinks/copies of `soldr`, not
as separate `src/bin` targets.
