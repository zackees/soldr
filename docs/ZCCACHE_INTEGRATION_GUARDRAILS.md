# Zccache Integration Guardrails

`contracts/zccache-integration-guardrails.v1.json` is the test and perf
ledger for the zccache refactor tracked by soldr issue #543. It exists so the
embedded runtime topology, wrapper environment, setup-soldr, release, npm, and
performance contracts are visible in one place instead of rediscovered from
scattered regression tests.

The ledger describes the compiled-in architecture introduced by issue #1368:
rustc and native compiler requests route through soldr-daemon's embedded
zccache service. It intentionally does not reference the removed managed
zccache download, external session lifecycle, or standalone-wrapper retry
tests. The no-standalone-daemon contract from issue #1467 is enforced by the
gated in-process `soldr zccache` entry point and its source lint.

## Gate Policy

Hard gates must pass before a zccache integration refactor wave merges. Report
only canaries must stay runnable and documented, but they are expected to run on
manual workflow dispatches or targeted perf branches rather than every PR.

If implementation finds a new soldr/zccache refactor area, file a child issue
and add it to the parent issue ledger before or alongside the implementation
PR. Do not fold unrelated refactors into the current wave without recording the
new work.

## Required Validation Commands

Run these hard gates for zccache integration refactor waves:

```powershell
soldr --no-cache cargo nextest run -p soldr-cli --test guards -E 'test(/^no_standalone_spawn_lint::/)'
soldr --no-cache cargo nextest run -p soldr-cli --test cargo_front_door -E 'test(/^zccache_trampoline_gate::/)'
soldr --no-cache cargo nextest run -p soldr-cli --test cargo_front_door -E 'test(/^cli_cargo_wrappers::/)'
soldr --no-cache cargo nextest run -p soldr-cli --test cargo_front_door -E 'test(/^cli_rust_plan::/)'
soldr --no-cache cargo nextest run -p soldr-cli --test cache_gc -E 'test(/^cli_cache::/)'
soldr --no-cache cargo nextest run -p soldr-cli --lib cache::session::tests
soldr --no-cache cargo nextest run -p soldr-cli --lib native_cc::tests
soldr --no-cache cargo nextest run -p soldr-cli --test cargo_front_door -E 'test(/^cli_cargo_native_cc::/)'
soldr --no-cache cargo nextest run -p soldr-cli --test cargo_front_door -E 'test(/^cli_wrapper_perf::/)'
soldr --no-cache cargo nextest run -p soldr-cli --test guards -E 'test(/^no_timed_test_guard::/)'
uv run --no-sync pytest tests/test_zccache_integration_guardrails.py tests/test_zccache_runtime_contract.py tests/test_setup_soldr_action.py tests/test_setup_soldr_exporter.py tests/test_setup_soldr_ensure_soldr.py -q
node scripts/test-npm-package.js
```

Since soldr#2934 the soldr-cli integration tests are grouped into a handful of
category targets (`guards`, `cargo_front_door`, `cache_gc`, …) whose sibling
`.rs` files are modules of that one target. `--test <category>` therefore
selects the whole category, so each guardrail command pairs it with an
`-E 'test(/^<module>::/)'` filter that narrows the run back down to the single
module the guardrail owns.

Run these report-only canaries when a wave touches performance-sensitive
zccache behavior:

```powershell
gh workflow run perf-cold-warm.yml -f run_mode='Purge cache and run cold build before warm build' -f fixture=medium
gh workflow run perf-matrix.yml -f platforms=linux -f fixtures=medium -f scenarios=all
```

## Guardrail Axes

- `embedded-runtime-topology`: locks the one-daemon architecture. Compile
  requests use the service embedded in soldr-daemon, `soldr zccache` enters the
  vendored CLI only through its gated in-process surface, and no soldr source
  may reach a standalone zccache-daemon spawn path.
- `embedded-session-env`: locks the cargo child environment and embedded
  compile-stat session summaries. `RUSTC_WRAPPER` routes through soldr,
  external zccache binary/session variables are cleared, and
  `ZCCACHE_PATH_REMAP` / `ZCCACHE_WORKTREE_ROOT` retain their documented
  propagation rules.
- `rust-plan-cache`: locks in-process rust-plan restore before Cargo and save
  after Cargo, the artifact bundle location, partial-restore diagnostics, and
  compile-count-driven warm-save decisions.
- `disabled-and-non-build`: locks cache-scoping on the two axes that must
  bypass the embedded compile service. A `--no-cache` (or `SOLDR_CACHE_ENABLED=0`)
  build reaches cargo without routing through the embedded zccache wrapper, and
  non-build cargo commands that do not compile preserve the documented wrapper
  policy instead of doing unnecessary cache/session work.
- `embedded-flush-shutdown`: locks command-lifetime durability through the
  embedded flush IPC, compile-stat finalization, soldr-daemon shutdown, and
  exact responder-generation tracking. A daemon that acknowledges shutdown is
  allowed to finish durability work and is never force-killed; timeout
  diagnostics remain bounded at the CLI boundary.
- `setup-action-outputs`: locks setup-soldr cache outputs, target-cache mode,
  target-cache keys, and native-cache policy output.
- `release-npm-staging`: locks release archive manifest validation, the
  embedded zccache runtime declaration, crgx staging, and npm-exported contract
  files.
- `perf-cold-warm`: keeps the cold/warm build and hit-rate workflow runnable.
- `perf-worktree-share`: keeps shared-cache worktree reuse measurable.
- `perf-touch-no-change`: keeps touch-without-change fingerprint churn
  measurable.
- `perf-build-then-check`: keeps cross-verb `build` to `check` cache reuse and
  the rustc `--emit` canonicalization canary measurable.
- `native-cc-cache`: locks native C/C++ env injection, the `zccache-soldr`
  shim, and opt-out behavior.
- `monolith-migration-ratchet`: ensures embedded-runtime integration tests
  remain discoverable outside monolithic source modules and that the removed
  per-test watchdog does not return (timeouts belong to nextest).

## Native Cache Guardrail

Issue #551 promoted the real `cli_cargo_native_cc` embedded-wrapper integration
test from report-only to a hard gate after the Windows build-script hang repro
passed on current main. They run under cargo-nextest's `slow-timeout` /
`terminate-after` (`.config/nextest.toml`) so a future build-script wrapper
hang is terminated and reported against that one test, instead of leaving the
suite stuck until external cleanup.
