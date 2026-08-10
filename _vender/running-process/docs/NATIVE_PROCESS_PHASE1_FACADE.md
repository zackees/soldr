# NativeProcess Migration Phase 1 Checkpoint

Date: 2026-04-08 (America/Los_Angeles)

## Goal

Introduce a unified native `NativeProcess` facade and route Python construction through it while preserving current behavior.

## Delivered

- Added Python-visible native facade class `NativeProcess` in `crates/running-process-py/src/lib.rs`.
- `NativeProcess` now encapsulates backend selection:
  - `Running(NativeRunningProcess)`
  - `Pty(NativePtyProcess)`
- Added `NativeProcess.for_pty(...)` constructor for PTY mode.
- Updated Python callers to route through the unified facade:
  - `src/running_process/running_process.py`
  - `src/running_process/pty.py`
- Updated compatibility test monkeypatch target to `NativeProcess`.

## Validation

- `python -m py_compile src/running_process/pty.py src/running_process/running_process.py src/running_process/running_process_manager.py tests/conftest.py tests/test_pty_support.py` passed.
- `python build.py` passed and wheel reinstall completed.
- `python -m pytest tests/test_pty_support.py -ra --durations=10`
  - `50 passed in 7.56s`
  - wall runtime: `7.942s`
- `python -m pytest tests/test_running_process.py -ra --durations=10`
  - `69 passed in 12.27s`
  - wall runtime: `12.637s`
- Traced run:
  - `python -m ci.run_logged logs/phase1/trace-phase-1-pty.log -- python -m pytest tests/test_pty_support.py -ra`
  - `50 passed in 7.36s`
  - wall runtime: `7.910s`

