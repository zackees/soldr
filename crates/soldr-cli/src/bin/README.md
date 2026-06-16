# soldr-cli binaries

Sidecar binaries that ship in the `soldr-cli` crate alongside the main
`soldr` binary defined in `../main.rs`.

- **`soldr_daemon.rs`** — `soldr-daemon` long-lived helper process that
  owns target/ tracking (phase 1: `start`, `stop`, `status`).
- **`soldr_shim.rs`** — per-tool shim binaries (`cargo`, `rustc`,
  `rustfmt`, `clippy-driver`, `rustdoc`) installed under
  `~/.soldr/v<X.Y.Z>/shims/` so soldr can interpose on bare tool
  invocations without rewriting every consumer's `PATH`.
