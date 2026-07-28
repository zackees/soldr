# `soldr-core::core`

Foundational types shared by every other soldr crate. `core` has no upward
edges — nothing here may depend on `fetch`, `cache`, `cli`, or `daemon`
(#1490 Phase 0). When a helper needs to be reachable from two crates that
would otherwise import each other, it belongs here.

| File | Responsibility |
|---|---|
| `mod.rs` | Module wiring and the crate's public re-export surface |
| `paths.rs` | `~/.soldr/` layout, `SoldrPaths`, `SoldrConfig` (`config.toml`), `resolve_cargo_home` / `resolve_rustup_home` |
| `temp.rs` | The scratch root — `SOLDR_TMPDIR`, defaulting to `<cache>/tmp` so it stays off `tmpfs` and on the cache's filesystem |
| `toolchain_manifest.rs` | Parsing `rust-toolchain.toml` into `RustToolchainManifest` |
| `toolchain_resolve.rs` | Locating `cargo`/`rustc` binaries; ancestor search; implicit toolchain homes |
| `target_triple.rs` | `TargetTriple` and its `Arch` / `Os` / `Env` components |
| `canonical_targets.rs` | The canonical target list and `is_canonical` |
| `git.rs` | Git metadata helpers |
| `wire.rs`, `wire_proto.rs`, `wire.proto` | Shared wire types. `wire_proto.rs` is **hand-written** prost; `wire.proto` is the schema of record and is kept in sync manually |

## Conventions

- Environment-variable names are `pub const *_ENV_VAR` declared next to the
  code that reads them, with a doc comment stating what the escape hatch is
  for. There is no central env-var registry.
- Any `SOLDR_*` variable is auto-forwarded to the daemon
  (`daemon/lifecycle.rs`), so a new one takes effect across the process tree
  without extra plumbing.
