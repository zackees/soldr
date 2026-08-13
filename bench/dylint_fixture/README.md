# dylint_fixture

Small, self-contained cargo workspace used by `bench/dylint_perf.py` to
measure cold vs warm `soldr cargo dylint` wall time. Built for soldr#1788
Phase 1.

## Layout

- `app/` — bin crate. Depends on `serde` (derive), the local `macros`
  proc-macro crate, and `dep_user`. Has a trivial `build.rs`.
- `macros/` — tiny proc-macro crate (`#[derive(Greet)]`) so proc-macro
  compilation is exercised, not just plain dependency compilation.
- `dep_user/` — lib crate pulling in `anyhow` + `serde_json` (moderate,
  non-trivial dependency compilation) and defining `forbidden_marker_fn`,
  the function the custom lint forbids calling.
- `lints/ban_forbidden_fn/` — a real Dylint 6.0.1 library (late lint pass)
  that denies any call to `dep_user::forbidden_marker_fn`. Modeled on
  zccache's `dylints/ban_tmp_literal`.

**Default state is lint-clean.** `app/src/main.rs` does not call
`forbidden_marker_fn`, so `cargo dylint --all --workspace` exits 0 against
this checkout as committed — cold/warm bench runs stay green.
`app/src/violation.rs.disabled` is a drop-in replacement for `main.rs` with
one live call to `forbidden_marker_fn`; `bench/dylint_perf.py --expect-fail`
swaps it in temporarily, asserts the lint fires, and restores the original
file (always, via try/finally).

## Toolchain

`rust-toolchain.toml` at this root pins stable `1.95.0`, matching the soldr
repo root — it does **not** pin a nightly. cargo-dylint 6.0.1 resolves the
driver toolchain for each `lints/*` library independently of this file, so
the app/dep_user/macros crates never need to touch nightly.

## Running

From this directory (or anywhere, cargo/soldr resolve the workspace root):

```bash
soldr cargo dylint --all --workspace
```

Or via the bench harness from the soldr repo root:

```bash
# Docker (soldr-perf-local container), cold -> warm -> optional warm-clean-target:
uv run --no-project python bench/dylint_perf.py

# Same, directly on the host:
uv run --no-project python bench/dylint_perf.py --host

# Prove the lint still fires after cache restores:
uv run --no-project python bench/dylint_perf.py --expect-fail
```

See `bench/dylint_perf.py` module docstring for the full scenario list and
flags.
