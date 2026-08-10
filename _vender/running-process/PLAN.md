# PTY Mandatory Plan

## Goal

Make PTY support a guaranteed capability of `running-process` on every supported platform.

This means:

- `Pty.is_available()` should effectively always be `True` on supported targets
- PTY-backed APIs should stop being treated as optional
- PTY tests should stop skipping on availability
- Windows should not depend on a fragile Python-only fallback story
- more PTY runtime logic should move into Rust where it improves portability and testability

## Current Problems

### API Problem

The package still models PTY as an optional feature:

- `Pty.is_available()`
- `PtyNotAvailableError`
- `@pytest.mark.skipif(not Pty.is_available(), ...)`

That leaks implementation uncertainty into the public API and weakens confidence in PTY-first features like:

- `wait_for_idle(...)`
- `wait_for(...)`
- `wait_for_expect(...)`
- constructor-registered `Expect`

### Platform Problem

Windows PTY is currently tied to `winpty` import availability in Python.

That is the wrong ownership boundary. PTY availability should be guaranteed by the package build/runtime, not by whether a Python dependency imported successfully at runtime.

### Testing Problem

A large part of the PTY test suite is skipped when PTY is unavailable. That means the most important interactive behavior is not part of the enforced contract.

## Target State

### User-Facing Contract

For supported platforms:

- PTY is always available
- `RunningProcess.pseudo_terminal(...)` is a core path, not an optional extra
- `wait_for_expect(...)` is safe to rely on everywhere
- `wait_for_idle(...)` is safe to rely on everywhere

For unsupported platforms:

- fail at install/build time, not late at runtime
- or explicitly declare the platform unsupported

Do not keep a "maybe PTY exists" runtime contract for platforms we claim to support.

## Design Direction

### 1. Move PTY ownership into Rust

Rust should own the platform-specific PTY backend choice.

Python should not decide:

- whether PTY exists
- which Windows PTY library to use
- how output capture is synchronized
- how stdin writes are merged with callback-generated input

Rust should expose one stable PTY abstraction to Python.

### 2. Unify backend semantics

The Rust layer should provide:

- PTY spawn
- PTY read loop
- PTY write
- resize
- interrupt / terminate / kill
- output history buffer
- incremental cursor/checkpoint support for `Expect`
- event stream or callback-safe signaling primitives for wait conditions

Python should remain responsible for:

- high-level `Expect` / `Idle` dataclasses
- wait condition orchestration policy
- API ergonomics

But the low-level terminal behavior should be Rust-owned.

### 3. Treat Windows as first-class

The Windows path must be as intentional as the POSIX path.

Options:

- use ConPTY from Rust directly
- use a Rust crate that wraps ConPTY robustly
- keep `winpty` only as a temporary compatibility bridge while ConPTY lands

Preferred direction:

- move to ConPTY-backed Rust implementation
- remove Python-level `winpty` dependency from the critical path

Reason:

- ConPTY is the modern Windows terminal API
- Rust can own the FFI and buffering model cleanly
- Python should not be responsible for terminal backend selection

## Concrete Work Plan

### Phase 1: Lock the contract

1. Define supported PTY platforms explicitly.
2. Decide whether unsupported platforms are:
   - build/install failures
   - or runtime import failures during module import
3. Update docs to say PTY is mandatory on supported platforms.
4. Stop describing PTY as optional capability in public docs.

Deliverable:

- a written support matrix in `README.md`

### Phase 2: Rust PTY backend abstraction

Create a Rust-side PTY abstraction with a common interface:

- `spawn(...)`
- `read_chunk(...)`
- `write(...)`
- `resize(...)`
- `send_interrupt(...)`
- `terminate(...)`
- `kill(...)`
- `poll_exit(...)`

Backends:

- POSIX PTY backend
- Windows ConPTY backend

Deliverable:

- Python no longer directly imports or talks to `winpty`

### Phase 3: Rust-owned PTY buffer and checkpoints

Move the remaining PTY data-path state fully into Rust:

- output history
- unread chunk queue
- checkpoint offsets
- maybe expect-search helpers if useful

This is especially important now that `Expect` has:

- late-bound matching concerns
- `after` checkpoints
- constructor registration
- sequential `wait_for_expect(next_expect=...)`

Deliverable:

- Python reads stable snapshots/checkpoints from Rust instead of reconstructing behavior from Python-side history assumptions

### Phase 4: Rust input merge path for callbacks

The new callback buffer behavior should be pushed down one layer.

Rust should support:

- queued stdin writes
- ordered merging of callback-generated writes
- safe serialization with user writes

Python should decide *what* to write.
Rust should own *how* writes are serialized into PTY stdin.

Deliverable:

- callback-generated writes do not race normal writes
- one consistent input ordering model across platforms

### Phase 5: Remove optional PTY runtime branching

Eliminate or sharply narrow:

- `Pty.is_available()` as a meaningful runtime branch
- `PtyNotAvailableError` in normal supported-platform usage
- Python backend-selection logic based on import success

Possible end state:

- `Pty.is_available()` remains only for compatibility and always returns `True` on supported builds
- or remove it in the next breaking release

Deliverable:

- PTY availability no longer controls test execution on supported CI

### Phase 6: Test policy change

Replace availability skips in PTY tests.

Current pattern:

```python
@pytest.mark.skipif(not Pty.is_available(), reason="PTY support is not available")
```

Target:

- no skip on supported platforms
- tests must fail if PTY setup is broken

Keep only platform-specific skips where behavior is genuinely different and documented.

Example:

- a temporary Windows-specific timing skip while exit ordering is normalized

But even those should be tracked as bugs, not treated as permanent normal state.

Deliverable:

- PTY suite is mandatory in CI

## Code Changes Required

### Python

- replace Python `winpty` dependency path in [pty.py](C:/Users/niteris/dev/running-process/src/running_process/pty.py)
- remove availability-based branching in [running_process.py](C:/Users/niteris/dev/running-process/src/running_process/running_process.py)
- simplify test assumptions in [test_pty_support.py](C:/Users/niteris/dev/running-process/tests/test_pty_support.py)

### Rust

- expand [crates/running-process-py/src/lib.rs](C:/Users/niteris/dev/running-process/crates/running-process-py/src/lib.rs) PTY surface
- likely add shared PTY backend code in `running-process` or a new Rust module/crate
- add Windows ConPTY implementation
- expose stable PTY lifecycle and buffer APIs to Python

## Testing Plan

### Unit / Integration

- PTY spawn on Windows and POSIX
- read/write round-trip
- resize
- interrupt behavior
- exit code propagation
- idle detection
- registered `Expect`
- `wait_for_expect(next_expect=...)`
- callback-generated stdin writes
- checkpoint-based matching

### CI Expectations

CI must run PTY tests on:

- Windows
- Linux
- macOS

PTY failures should be release blockers.

## Migration Notes

### Short term

Keep compatibility helpers if needed:

- `Pty.is_available()`
- `PtyNotAvailableError`

But treat them as legacy shims, not core design points.

### Medium term

Deprecate:

- runtime optionality semantics for PTY
- Python-owned Windows backend selection

### Long term

Document PTY as part of the package identity:

- `running-process` guarantees interactive PTY process support everywhere it claims support

## Immediate Next Steps

1. Choose the Rust Windows backend: direct ConPTY FFI or a Rust crate wrapper.
2. Implement Rust PTY spawn/read/write on Windows behind the same abstraction used by POSIX.
3. Remove Python `winpty` as the source of truth for PTY availability.
4. Convert PTY tests from availability-skipped to mandatory on supported platforms.
5. Keep only narrow, explicitly documented platform-specific timing skips until semantics are fully aligned.
