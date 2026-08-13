# Blessed IPC transport boundary

`ban_raw_ipc_transport` rejects raw local-socket and named-pipe construction
outside Soldr's internal IPC adapter modules. Callers use the stable facades;
only these adapters may touch `interprocess`, Tokio/std Unix socket
constructors, or Tokio named-pipe constructors directly:

- `crates/soldr-daemon/src/daemon/client.rs`
- `crates/soldr-daemon/src/daemon/server.rs`
- `crates/soldr-daemon/src/daemon/ipc_peer.rs`
- `crates/soldr-daemon/src/daemon/session_endpoint.rs`
- `crates/soldr-cli/src/broker_server.rs`
- `crates/soldr-cli/src/broker_spawn.rs`
- `crates/soldr-cli/src/broker_control_transport_{unix,windows}.rs`
- `crates/soldr-cli/src/session_transport.rs`

The public/internal facade for a capability must be platform-neutral. Private
implementations may be cfg-selected for macOS, Linux, or Windows, but must keep
the same signature. When a dependency operation is unavailable on one host,
the platform adapter supplies the native equivalent (for example, bind then
`chmod(0600)` on macOS).

Run the focused UI test with:

```console
cd dylints/ban_raw_ipc_transport
soldr cargo test --manifest-path Cargo.toml
```
