# soldr-cli binaries

`soldr` in `../main.rs` is the crate's only compiled binary target.
The daemon and embedded zccache entrypoints live in `../daemon_entry.rs`
and `../zccache_entry.rs` and are selected by argv[0].

The toolchain, clang, and `zccache-soldr` shim names are multicall names
for `soldr` itself and are installed as hardlinks/copies of `soldr`, not
as separate `src/bin` targets.
