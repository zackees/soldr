# Canonical local-socket name boundary

`ban_raw_local_socket_name` rejects direct `interprocess` `to_ns_name` and
`to_fs_name` calls in Soldr CLI, daemon, and vendored running-process
production code. Resolved endpoint strings must go through
`running_process::broker::server::singleton_bind::wrap_socket_name`, which
normalizes an already-resolved Windows `\\.\pipe\...` path exactly once.

Run its UI tests with:

```console
cargo test --manifest-path dylints/ban_raw_local_socket_name/Cargo.toml
```
