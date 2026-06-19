# Zccache Integration Guardrails

`contracts/zccache-integration-guardrails.v1.json` is the test and perf
ledger for the zccache refactor tracked by soldr issue #543. It exists so the
runtime/source, session, wrapper, setup-soldr, release, npm, and perf contracts
are visible in one place instead of rediscovered from scattered regression
tests.

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
soldr --no-cache cargo test -p soldr-cli --test cli_zccache_contract_matrix --locked
soldr --no-cache cargo test -p soldr-cli --test cli_install_zccache_resolution --locked
soldr --no-cache cargo test -p soldr-cli --test cli_cargo_wrappers --locked
soldr --no-cache cargo test -p soldr-cli --bin soldr --locked native_cc::tests
soldr --no-cache cargo test -p soldr-cli --test cli_cargo_native_cc --locked
soldr --no-cache cargo test -p soldr-cli --test cli_unknown_session_retry --locked
soldr --no-cache cargo test -p soldr-cli --test cli_wrapper_perf --locked
soldr --no-cache cargo test -p soldr-cli --test timed_test_lint --locked
uv run pytest tests/test_zccache_integration_guardrails.py tests/test_zccache_runtime_contract.py tests/test_setup_soldr_action.py tests/test_setup_soldr_exporter.py tests/test_setup_soldr_ensure_soldr.py -q
node scripts/test-npm-package.js
```

Run these report-only canaries when a wave touches perf-sensitive zccache
behavior:

```powershell
gh workflow run perf-cold-warm.yml -f run_mode='Purge cache and run cold build before warm build' -f fixture=medium
gh workflow run perf-matrix.yml -f platforms=linux -f fixtures=medium -f scenarios=all
```

## Guardrail Axes

- `source-precedence`: locks the zccache source-resolution order across test
  overrides, local development builds, pinned managed runtime, managed cached
  runtime, and system fallback diagnostics.
- `managed-session-env`: locks managed daemon/session startup and the cargo
  child environment: `ZCCACHE_SESSION_ID`, default absence of
  `ZCCACHE_CACHE_DIR`, `ZCCACHE_PATH_REMAP`, and `ZCCACHE_WORKTREE_ROOT`.
- `rust-plan-cache`: locks zccache `rust-plan restore` before Cargo and
  `rust-plan save` after Cargo, while keeping zccache's active daemon/cache
  behavior separate from the target artifact bundle path.
- `disabled-and-non-build`: locks `--no-cache` and non-build cargo commands so
  they propagate `SOLDR_CACHE_ENABLED=0` and do not start managed zccache.
- `unknown-session-retry`: locks the Windows wrapper recovery path for
  `zccache error: unknown session:` so soldr retries once with a fresh session.
- `shutdown-scoping`: locks command-lifetime shutdown so `session-end` happens
  before daemon stop and the stop targets the soldr-owned zccache root.
- `setup-action-outputs`: locks setup-soldr cache outputs, target-cache mode,
  target-cache keys, and native-cache policy output.
- `release-npm-staging`: locks release archive manifest validation, bundled
  zccache/crgx binaries, and npm package contract files.
- `perf-cold-warm`: keeps the cold/warm build and hit-rate workflow runnable.
- `perf-worktree-share`: keeps shared-cache worktree reuse measurable.
- `perf-touch-no-change`: keeps touch-without-change fingerprint churn
  measurable.
- `native-cc-cache`: locks native C/C++ env injection and opt-out behavior.
- `monolith-migration-ratchet`: ensures new zccache integration tests are
  discoverable outside monolithic source modules and use the `timed_test`
  watchdog.

## Native Cache Guardrail

Issue #551 promoted the real `cli_cargo_native_cc` managed-wrapper integration
test from report-only to a hard gate after the Windows build-script hang repro
passed on current main. Those tests now run under `timed_test!` so a future
build-script wrapper hang fails with a bounded watchdog instead of leaving the
suite stuck until external cleanup.
