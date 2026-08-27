# soldr-cli binaries

`soldr` in `../main.rs` is the crate's only compiled binary target.
The daemon entrypoint lives in `../daemon_entry.rs` and is selected by
argv[0]. `soldr zccache` is a Soldr-owned compatibility surface; it does not
embed an upstream zccache CLI dispatcher.

The toolchain, clang, and `zccache-soldr` shim names are multicall names
for `soldr` itself and are installed as hardlinks/copies of `soldr`, not
as separate `src/bin` targets.
