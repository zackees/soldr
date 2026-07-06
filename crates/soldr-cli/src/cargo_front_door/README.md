# cargo_front_door

The `soldr cargo ...` front door: everything soldr does before, around, and
after spawning the child `cargo` process.

- `mod.rs` — entry point (`run_cargo_front_door`), cacheability decisions,
  cargo argument parsing, build-session correlation.
- `cache_plan.rs` — `CargoCachePlan`: resolves the `RUSTC_WRAPPER` plan,
  injects native-C caching (via the `zccache-soldr` shim → embedded daemon,
  soldr#1368), and drives rust-artifact save/restore (`rust_plan.rs`).
- `inputs.rs` / `target.rs` / `subcommand.rs` / `profile_debug.rs` — cargo
  argument + environment analysis feeding the cache plan.
- `component_install.rs` / `cook_hydrate.rs` / `disk.rs` — pre-build
  toolchain/component ensure, cook hydration, and disk-space guards.
- `clang_cl_shim.rs` / `zig_shim.rs` — cross-compile linker/compiler shims.

As of soldr#1368 the front door no longer downloads a managed zccache binary
or spawns a separate zccache daemon: rustc compiles route through the
soldr-daemon embedded zccache service via `RUSTC_WRAPPER=soldr`, and native-C
compiles via the `zccache-soldr` shim. The build session is a parent-side
carrier for the shared zccache cache dir + rust-plan paths.
