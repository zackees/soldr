# Native Rust PTY / Terminal Input Refactor Plan

## Goal

Move PTY and terminal-input control out of the Python layer and make Rust the owner of interactive terminal behavior.

After this refactor:

- Rust owns PTY lifecycle and data flow
- Rust owns terminal input capture, translation, relay, and restoration
- Rust owns terminal mode changes and parent-console passthrough details
- Python becomes a thin API wrapper over native behavior

This is an ownership refactor first, not just a code shuffle. The main objective is to remove correctness-critical terminal orchestration from `src/running_process/pty.py`.

## Current Problem

Too much PTY behavior is still coordinated in Python:

- Windows terminal input relay startup and shutdown
- relay worker lifecycle
- event forwarding into the PTY
- idle arming based on submit events
- echo-to-console passthrough
- parent-console VT-output enablement
- parts of output and activity accounting

That creates the wrong boundary:

- correctness-sensitive interactive behavior is split across Rust and Python
- console state changes are not owned by the same layer that captures input
- PTY behavior is harder to reason about and test end-to-end
- downstream tools like `clud` depend on Python control flow for terminal correctness

## Target Ownership Boundary

Rust should own:

- PTY spawn, resize, read, write, interrupt, terminate, kill
- terminal input capture lifecycle
- console mode changes and restoration
- key event translation
- PTY input relay thread or equivalent loop
- input accounting:
  - input bytes
  - newline count
  - submit count
- output accounting:
  - output bytes
  - control churn bytes
  - last activity timestamp
- optional parent-console output preparation on Windows
- structured terminal-session shutdown semantics

Python should own:

- public API shape
- argument normalization
- high-level policy choices that are intentionally user-facing
- compatibility shims during migration

Python should not own:

- console handles
- console mode mutation
- relay worker threads
- key translation rules
- direct PTY input relay logic
- correctness-critical interactive state

## Refactor Principles

1. Move ownership, not just implementation.
2. Keep one source of truth for terminal state.
3. Preserve current public behavior while migrating internals.
4. Add tests before deleting Python logic.
5. Prefer native lifecycle objects over scattered helper functions.

## Phase 1: Inventory And Boundary Lock

### Deliverables

- inventory of Python-owned PTY and terminal-input behavior
- explicit classification for each behavior:
  - native-owned
  - policy-owned
  - temporary shim

### Work

- audit `src/running_process/pty.py`
- identify all PTY/relay-related mutable state
- identify all Python worker threads and console state mutations
- map current Rust responsibilities in `crates/running-process-py/src/lib.rs`
- document every cross-boundary interaction between Python and Rust

### Exit Criteria

- there is a written ownership map
- every Python PTY/input helper is marked for keep, move, or delete

## Phase 2: Native Terminal Session Abstraction

### Deliverables

- one Rust-side abstraction for an interactive PTY session

Suggested shape:

- `NativeTerminalSession` or equivalent native-owned state object

Responsibilities:

- PTY process handle ownership
- optional terminal input capture ownership
- optional relay ownership
- output history and activity accounting
- shutdown and restoration behavior

### Work

- define the Rust struct and lifecycle model
- decide whether to extend `NativePtyProcess` or wrap it
- ensure the abstraction can support Windows first and POSIX after

### Exit Criteria

- there is one clear native session owner for PTY + input relay behavior
- Python no longer needs to coordinate multiple native primitives directly

## Phase 3: Move Windows Terminal Input Relay Into Rust

### Deliverables

- Rust-owned input relay lifecycle on Windows

### Work

- move `NativeTerminalInput.start/stop` orchestration behind the session object
- move relay worker startup and shutdown into Rust
- move PTY writes for translated key events into Rust
- keep `submit` detection native-owned
- keep console mode restoration native-owned even on abnormal shutdown paths
- preserve current Enter vs Shift+Enter semantics

### Exit Criteria

- Python no longer starts a relay thread for Windows PTY input
- Python no longer loops over `read_event(...)`
- Rust owns the full capture-to-PTY-input path

## Phase 4: Move Parent Console Output Handling Into Rust

### Deliverables

- Rust-owned output passthrough support for interactive PTY echo mode

### Work

- move parent-console VT-output preparation into Rust on Windows
- define whether native echo writes directly to the console or exposes a prepared stream for Python
- prefer native ownership if passthrough correctness depends on console mode handling
- preserve raw ANSI passthrough behavior needed by TUI apps like Codex

### Exit Criteria

- Python does not need to enable VT mode itself for PTY echo correctness
- ANSI passthrough correctness is guaranteed natively

## Phase 5: Move Activity And Idle Primitives Into Rust

### Deliverables

- native-owned activity counters and timestamps

### Work

- move input/output accounting into Rust
- track:
  - bytes written to PTY
  - newline events
  - submit events
  - bytes read from PTY
  - control-churn bytes
  - last activity timestamp
- expose snapshots or structured counters to Python

### Exit Criteria

- Python stops updating PTY activity counters manually
- idle-related state comes from a native source of truth

## Phase 6: Narrow The Python API Surface

### Deliverables

- a smaller Python wrapper over the native session object

### Work

- replace Python relay methods with native delegation
- keep high-level API methods stable where possible:
  - `start_terminal_input_relay(...)`
  - `stop_terminal_input_relay()`
  - `write(...)`
  - `submit(...)`
  - `send_interrupt()`
  - `resize(...)`
  - `read(...)`
  - `drain(...)`
  - `wait(...)`
- reimplement these as thin wrappers
- remove Python-side ownership of interactive thread state and console restoration

### Exit Criteria

- `pty.py` is mostly argument validation, compatibility, and API wiring
- terminal correctness no longer depends on Python threading behavior

## Phase 7: Compatibility Shim Pass

### Deliverables

- temporary compatibility layer that preserves callers while internals change

### Work

- keep Python method names stable during migration
- route old codepaths into the new native session object
- deprecate internal-only Python PTY helpers that should disappear

### Exit Criteria

- callers like `clud` do not need to change immediately
- old Python internals are no longer the active implementation

## Phase 8: Test Migration

### Rust tests

- console input mode tests
- key translation tests
- submit vs Shift+Enter tests
- key-up filtering tests
- relay lifecycle tests
- console mode restoration tests
- parent-console passthrough tests where feasible

### Python tests

- API compatibility tests
- PTY wrapper behavior tests
- integration tests that verify native behavior through Python APIs

### End-to-end tests

- PTY interactive launch tests
- real Codex smoke test on Windows
- interrupt and cleanup behavior tests
- idle detection behavior tests

### Exit Criteria

- correctness-critical relay behavior is covered at the Rust layer
- Python tests verify API behavior, not terminal internals

## Phase 9: Delete Old Python Control Logic

### Work

- remove Python relay threads
- remove Python console-mode handling
- remove Python VT-output enablement
- remove Python-owned PTY activity bookkeeping that now duplicates Rust
- simplify `PseudoTerminalProcess`

### Exit Criteria

- Python is no longer the owner of PTY/input control
- duplicated state is removed
- the codebase reflects the intended architecture

## Risks

- Windows console behavior can vary by host
- PTY echo behavior may differ between direct console writes and buffered passthrough
- idle semantics can drift if accounting moves in pieces instead of as a whole
- temporary mixed ownership can create subtle regressions if migration phases overlap too long

## Risk Controls

- migrate one ownership slice at a time
- keep temporary shims thin
- prefer native tests for terminal correctness
- use opt-in trace instrumentation while migrating
- validate with real Windows console hosts, not only mocked tests

## Suggested Implementation Order

1. Lock ownership boundaries in writing.
2. Introduce the native terminal session abstraction.
3. Move Windows terminal input relay into Rust.
4. Move parent-console VT/output handling into Rust.
5. Move activity and idle primitives into Rust.
6. Collapse Python wrappers onto the native session.
7. Remove obsolete Python control logic.
8. update docs and architecture notes.

## Build And Verification Workflow

This project is supposed to be built through the repo tooling, not by ad hoc direct commands alone.

The build path is intentionally more complicated because it handles:

- the repo-local Python environment
- the Rust toolchain bootstrap
- Windows Visual Studio build environment setup
- dev-wheel reinstall behavior
- release-wheel artifact shaping
- Windows tiny-PDB packaging and verification

### Canonical build entrypoints

Use these entrypoints from the repository root:

- dev build:
  - `python build.py --dev`
- release build:
  - `python build.py --release`
- default build script behavior:
  - `python build.py`
  - this routes to the project build helper and defaults to the configured mode

In practice the repo expects the local `.venv` interpreter when available.

### What the build actually does

The build flow goes through:

- [build.py](C:/Users/niteris/dev/running-process/build.py)
- [ci/build_wheel.py](C:/Users/niteris/dev/running-process/ci/build_wheel.py)
- [ci/env.py](C:/Users/niteris/dev/running-process/ci/env.py)

That means:

- `maturin` is invoked through Python, not treated as the primary manual interface
- the build environment is normalized through `build_env()`
- `VIRTUAL_ENV` is cleaned to avoid stale environment leakage
- on Windows, the build logic attempts to load:
  - the pinned rustup toolchain from `rust-toolchain.toml`
  - the host target triple
  - Visual Studio `VsDevCmd.bat`

### Dev build behavior

The intended dev path is:

1. build a dev-profile wheel into `dist/`
2. reinstall that wheel into the active repo environment
3. remove stale editable-install `.pth` artifacts if present

There is also a fingerprinted dev-wheel cache path through:

- [ci/dev_build.py](C:/Users/niteris/dev/running-process/ci/dev_build.py)

That path:

- computes a source fingerprint
- reuses the last compatible wheel when possible
- otherwise rebuilds the wheel
- reinstalls the resulting wheel into the repo interpreter

This is the path CI/test helpers rely on when they need to ensure the native extension matches the current source tree.

### Release build behavior

The intended release path is not just `maturin build --release`.

On release builds the repo tooling additionally handles:

- platform compatibility flags
  - Linux uses `manylinux2014`
  - non-Linux uses `pypi`
- Windows tiny-PDB handling
- symbol filtering
- release artifact verification

On Windows the release flow applies the tiny-PDB pipeline before considering the artifact complete.

### Test execution path

Tests are also expected to run through the repo entrypoint rather than raw `pytest`.

Use:

- `uv run --no-editable -m ci.test`
- targeted example:
  - `uv run --no-editable -m ci.test tests/test_pty_support.py windows_native_input_relay`

Reasons:

- it ensures the current dev wheel is built and installed
- it sets the expected `IN_RUNNING_PROCESS=running-process-cli`
- it runs native and Python tests under the intended supervision path
- it enables timeout diagnostics and stack-dump collection

For one-off targeted runs that need the same supervision behavior, the lower-level wrapper is:

- `python -m running_process.cli --timeout 90 -- python -m pytest ...`

### Direct cargo usage

Direct `cargo test --workspace` is useful and expected during native development, but it is not the full product build path by itself.

When using direct cargo commands:

- prefer the repo `.venv` Python as `PYO3_PYTHON`
- ensure `VIRTUAL_ENV` points at `.venv` or is cleared
- avoid stale external virtualenv paths like `venv`

On Windows, a reliable form is:

1. set `PYO3_PYTHON` to `.venv\\Scripts\\python.exe`
2. set `VIRTUAL_ENV` to `.venv`
3. run `cargo test --workspace`

### Build rules for this refactor

During the native PTY/input refactor, every major phase should be validated with:

1. `cargo fmt`
2. `cargo test --workspace`
3. targeted PTY Python tests through the supervised runner
4. at least one dev wheel rebuild through the canonical build path

Before calling a migration phase complete on Windows, also validate:

- a dev wheel install from `build.py --dev`
- a release build path from `build.py --release` if native symbols or exported behavior changed materially

## Definition Of Done

This refactor is done when:

- PTY and terminal-input control are native-owned
- Python no longer coordinates relay threads or console modes
- interactive correctness survives without Python terminal orchestration
- Rust tests cover terminal correctness directly
- Python remains a thin wrapper over the native engine
