# NativeProcess Migration Phase 0 Baseline

Date: 2026-04-08 (America/Los_Angeles)

## Scope

Phase 0 objective: capture baseline behavior, baseline runtime, and target test coverage before deeper NativeProcess migration.

## Baseline Behavior Inventory

Critical public behavior areas covered in baseline tests:

- `RunningProcess` subprocess lifecycle: start, wait, timeout, kill/terminate, interrupt handling, output capture, stream-specific reads.
- `PseudoTerminalProcess` PTY lifecycle: interactive I/O, expect/wait_for flows, idle detection modes, EOF/exit handling, kill/terminate idempotency.
- `InteractiveProcess` console interactive modes, interrupt and exit semantics.

## Targeted Coverage and Results

- PTY suite: `tests/test_pty_support.py` -> `50 passed`.
- Subprocess suite: `tests/test_running_process.py` -> `69 passed`.
- Live lifecycle suite: `tests/test_live_process_behavior.py` with `RUNNING_PROCESS_LIVE_TESTS=1` -> `3 passed`.

## Baseline Runtime (Non-traced)

- `python -m pytest tests/test_pty_support.py -ra --durations=10`
  - pytest runtime: `7.68s`
  - wall runtime: `8.073s`
- `python -m pytest tests/test_running_process.py -ra --durations=10`
  - pytest runtime: `12.43s`
  - wall runtime: `12.799s`
- `python -m pytest tests/test_live_process_behavior.py -ra --durations=10`
  - pytest runtime: `0.66s`
  - wall runtime: `1.004s`

## Build/Install Gate

- `python -m py_compile src/running_process/pty.py src/running_process/running_process.py` passed.
- `python build.py` passed and reinstalled wheel `running_process-3.0.3-cp313-cp313-win_amd64.whl`.

## Trace/Diagnostics Baseline

- Traced baseline command:
  - `python -m ci.run_logged logs/phase0/trace-phase-0-pty.log -- python -m pytest tests/test_pty_support.py -ra`
- Result:
  - `50 passed in 7.33s`
  - wall runtime: `7.869s`
- Artifact preserved:
  - `logs/phase0/trace-phase-0-pty.log`

## Stabilization Note

- During Phase 0 validation, `test_pseudo_terminal_wait_for_idle_uses_callable_predicate` exposed a hang caused by `wait_for_idle` loop logic being accidentally nested under `if echo_output`.
- Fix applied in `src/running_process/pty.py` by restoring loop-body indentation so idle sampling and timeout/exit checks run regardless of `echo_output`.
