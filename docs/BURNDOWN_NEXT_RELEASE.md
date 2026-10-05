# Burn-down list — next release (10 verified bugs)

Every row was confirmed against `main` at `435a9fb1` by reading the code, not by
trusting the issue text. Rows 1-7 are existing issues re-verified; rows 8-10 include
findings from the 2026-10-05 triage. Several issues previously believed open turned
out to be already fixed and were dropped — see "Rejected candidates" at the bottom.

Ranked by user impact x confidence. `file:line` is the confirming evidence on `main`.

| # | Bug | Impact | Confirmed at | Issue |
|---|-----|--------|--------------|-------|
| 1 | `soldr save` dies on a file that vanishes mid-walk | Archive/save fails outright | `crates/soldr-cache/src/cache_lib/save_inventory.rs:737` | #3533 |
| 2 | Wrapper-coherence guard rejects valid `cargo-llvm-cov` chains | `soldr cargo llvm-cov` unusable on >= 0.9.21 | `crates/soldr-cli/src/wrapper_identity.rs:81-100` | #3504 |
| 3 | Dylint driver install is not atomic despite its own doc claiming it is | Concurrent worktrees fail | `crates/soldr-fetch/src/fetch/toolchain_packaged.rs:206-211` | #3538 |
| 4 | `cargo dylint` under shims cannot find `lib<name>@<toolchain>.so` | Lint lane broken | `crates/soldr-cli/src/cargo_front_door/run_prepare.rs:563` | #3483 |
| 5 | Tool dir prepended to PATH dirties pyo3 build scripts | Unnecessary rebuilds | `crates/soldr-cli/src/cargo_front_door/run_prepare.rs:521-525` | #3485 |
| 6 | Cache janitor can never evict superseded-toolchain entries | CI cache-budget failures | `.github/scripts/check_cache_budget.py:647` | #3545 |
| 7 | Failed local-gate logs truncated to the last 20 KB, permanently | Undiagnosable gate failures | `ci/local_gate.py:735-748`, `:887` | #3564 |
| 8 | Cache summary reads a stale, unkeyed stats file | Misleading hit-rate reporting | `crates/soldr-cli/src/cargo_front_door/cache_states.rs:120,180` | #3540 |
| 9 | Installer host-contract tests fail inside any container | Tests cannot run in managed CI | `install:29-31` + `tests/test_install_project_env.py:48-51` | #3565 |
| 10 | rustup toolchains are never GC'd (`purge` is report-only) | Unbounded `~/.soldr` growth | `crates/soldr-cli/src/soldr_main_commands.rs:414-429` | #3507 |

## Notes per row

**1 — `soldr save` TOCTOU (#3533).** `cache_file_entry` tolerates `NotFound` from
`std::fs::metadata`, then calls `hash_file(abs)?` on the very next line. A file
renamed away between the stat and the hash fails the whole save with
`SaveLoadError::Io`. `save_archive.rs:482` (`File::open` in a later parallel pass)
has the same window. `path_is_transient_runtime_file`
(`save_inventory.rs:490-501`) filters only `staging/`, `*.lock`, `*.sock`, `*.pid`,
so `.metadata.bin.tmp-*` is walked and hashed.
Note: #1942's fix was `perf/lib/common.sh` (bash), not a shared Rust helper, so
there is nothing to "apply" here — the Rust guard is a separate partial mitigation.

**2 — `cargo-llvm-cov` chaining (#3504).** `assert_inherited_wrapper_coherent`
compares `RUSTC_WRAPPER` to the soldr mirror unconditionally; the only exemptions
are an absent mirror and a `disabled` origin. cargo-llvm-cov legitimately sets
`RUSTC_WRAPPER` to itself and stashes the prior value in
`__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING`. No such allowlist exists anywhere in
the tree. Fires at `soldr_main.rs:243`, `cargo_front_door/run.rs:74`,
`soldr_main_external.rs:398`. The module's own doc claims "Soldr asserts only what
Soldr owns" — the exact case it fails to honor.

**3 — Dylint driver install (#3538).** `install_driver_file_atomically` deletes the
destination before renaming, so the destination is absent for that window and a
concurrent reader sees ENOENT. Locking does not cover it: `lock_driver_build` is
acquired only in the tier-2 source-build path, never around the tier-1 catalogue
install or the reader. The correct pattern already exists at
`dylint_driver/local_build.rs:418-431` and
`ci_test/dylint_library_marker.rs:484-497` (plain rename, fall back to
remove-then-rename).

**4 — dylint `lib@<toolchain>.so` (#3483).** `apply_linker_override` runs
unconditionally for every subcommand, injecting `CARGO_TARGET_<triple>_LINKER` and
`_RUSTFLAGS`. Injected RUSTFLAGS env *replaces* the config's `-C linker=dylint-link`,
so `dylint-link` never runs and no `@<toolchain>` copy is made. The existing
suppression guard misses twice: it reads the workspace-root `Cargo.toml` rather than
the per-lint `dylints/<lint>/.cargo/config.toml`, and it ignores `[target.'cfg(all())']`
sections — which is exactly how every dylint crate declares its linker.
**Caveat:** the final hop (that the nested cargo-dylint build inherits the env rather
than stripping it) is unverified. Needs a cheap RED fixture: declare the linker under
an exact `[target.<triple>]` section in a fixture lint crate.

**5 — PATH dirties pyo3 (#3485).** The fetched binary's parent dir is pushed into
`extra_bin_dirs` and prepended to PATH unconditionally — no stabilization, no dedup.
The dir is version-scoped (`cargo-chef-0.1.73` vs `cargo-nextest-<ver>`), so PATH
changes between phases. Both `cook` and `nextest` route through the same
`run_cargo_front_door`.

**6 — cache janitor (#3545).** *Needs a policy decision, not a patch.*
`action_store_lineage_candidates` evicts only when `lock != current_main_lock`, but
the reported entries share a lock hash. Its `shape` for the build key is the whole
`setup-soldr-buildcache-v2-<os>-<arch>-<toolchain>` segment, so a differing toolchain
hash is structurally a different lineage and `shape in current_shapes` never matches.
`setup-soldr-action-stores` also declares no `evict` policy in `ci/cache-ownership.json`,
so the 1.30 GiB allocation is unreachable by the janitor at all.
`tests/test_cache_budget.py:1248-1268` currently *requires* the opposite of the fix.

**7 — local-gate log truncation (#3564).** `run_captured` writes to a
`tempfile.TemporaryFile`, reads it into memory, and the `with` block unlinks it.
`main()` then prints only `result.output.rstrip()[-20000:]`. Everything past 20 KB is
permanently lost, and no durable log path is printed. The acceptance criterion in the
issue already specifies the fix shape: persist to the resolved git dir, print an exact
path, keep console output bounded and the return code unchanged.

**8 — stale cache summary (#3540).** The summary is read from one fixed path with no
session identity or freshness check. `finalize_build_session_stats` early-returns
without writing when the daemon is unreachable at end, but `emit_build_stats` still
prints whatever the file holds — so a no-op finalize prints the previous invocation's
numbers verbatim. Part 2 of the same issue is cleanly confirmed: `Outcome::parse` has
only `Hit`/`Miss`, so any record zccache labels `miss` renders as `[MISS]`, including
test-harness compiles zccache never caches by design.
*Caveat:* the issue's claim that "every later invocation repeats" is overstated — a
successful finalize does rewrite the file, and an empty invocation is suppressed.

**9 — installer host-contract tests (#3565).** `in_container()` checks
`/.dockerenv` / `/run/.containerenv` / `$container`, but the test fixture strips only
`UV_PROJECT_ENVIRONMENT`, `VIRTUAL_ENV`, and `container` from the environment. It
cannot emulate a host filesystem, so inside a real Docker-backed runner the two
host-contract tests hit the container refusal at `install:33` before reaching the host
path they assert. Confirmed empirically: `/.dockerenv` is present in the bosn
container where these tests run. Production behavior is correct; the fixture is
incomplete.

**10 — toolchain GC (#3507).** `gc purge --kind rustup_toolchain` errors with
"report-only; cargo/rustup own deletion", and `toolchain.rs` only ever installs. No
uninstall or prune path exists. Note the issue's other two root causes are wrong:
zccache *does* have a real LRU budget (5% of volume, clamped 40-200 GiB), and
`~/.soldr` vs `~/.soldr-dev` is intentional (avoids a daemon collision, #1597).
The 93 GB figure is explained by a 1.8 TB disk defaulting the budget to ~90 GiB.

## Filed after this list was written (verified, but outside the top 10)

**#3578 — `cargo_front_door` wrapper tests need a clang reachable from the
isolated PATH.** Filed 2026-10-05. Verified on `main`: ten tests in
`crates/soldr-cli/tests/cargo_front_door/cli_cargo_wrappers.rs` set
`PATH` to `isolated_test_path()` (`tests/common/mod.rs:2002`, applied at
e.g. `:440`), and the linker shim then has no `/usr/bin/clang`. The managed-LLVM
leg cannot rescue it because `fetch/llvm.rs:260` requires
`CatalogueSource::CanonicalV2` and the fallback (`linker_shim.rs:343`
`pick_after_managed`) needs a system clang on the test's PATH. Test-only (like
row 9) — production resolution is correct — but it fails the local gate's
`tests` lane on any host without `/usr/bin/clang`. Not counted in the 10
because its blast radius is the test suite, not users.

## Rejected candidates (do not re-triage)

Verified already fixed on `main`:
- **#3327** `make_executable` world-writable — fixed to a fixed 0o755, with a
  regression test that reproduces the umask-0000 base.
- **#3274** `dylint-link` MSVC probe — `validate_dylint_link_path_binary` now uses
  the one correct predicate and captures output.
- **#3277** linker/rustflags overriding project config — guard now present.
- **#2924** nested-Cargo self-locks — `nested_cargo_guard` is fully wired (it was
  reported as merged-but-unwired in the 0.9.20 triage; that is no longer true).
- **`verify_vendor_state.py`** — deleted; the inert script is gone.
- **#3503** `ZCCACHE_CACHE_SIZE_BYTES` inert — the knob is read
  (`zccache_embedded.rs:253`) and drives a real 300 s maintenance loop
  (`maintenance.rs:464`).
- **#3572** broker route sockets never reaped — fixed by #3573 (`435a9fb1`).

Needs more evidence (not a confirmed bug):
- **#3558** cook `WouldBlock` — the error is an expired `SO_RCVTIMEO` on the *read*
  (there are zero non-test `set_nonblocking` calls), not a write-side EAGAIN. The
  real gap is that `cook_record`'s error carries no stage, so "read deadline expired"
  and "allocation failed" are indistinguishable. Two `cook_dylint` flakes were
  observed on clean `main` and did not reproduce on the #3573 branch.

Not bugs:
- **#3499** cook-index pack cost — a valid perf request, not a defect. The pack is
  correct and idempotent; six skip conditions already exist, none CI-aware.
- **#3435** musl host-target wheel — intended and pinned by
  `wheel_cmd_tests.rs:333-341`.

## Two open environment defects (not soldr bugs)

- **bosn `.dockerignore` is not applied before the entry count.**
  `bosn-generation/src/collector.rs` enumerates every node, capped at `max_entries`
  (100_000), and only then applies ignore rules. A soldr checkout with a 16 GB
  `target/` reaches 108,811 nodes and `bosn run` refuses with
  `manifest Dockerfile context was refused`; a clean worktree (66,137) works.
  Unfiled — worth an issue against `zackees/bosn`.
- **A stale soldr daemon holds `~/.soldr`.** A v0.9.28 daemon whose PID had already
  exited kept the root lock, failing compiles of every crate including third-party
  build scripts. `soldr daemon stop` clears it. Misleading because the failure
  surfaces as unrelated `error: could not compile <third-party crate>`.
