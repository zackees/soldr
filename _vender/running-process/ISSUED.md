# Issues To Track

These items came out of studying a downstream `clud` regression, but only the library-level problems are listed here.

What does **not** belong here:

- `clud` should use subprocess mode for one-shot prompt/message runs.
- `clud` needs backend-specific Codex command building such as `codex exec ...` and `codex resume ...`.

Those are downstream CLI policy issues, not `running-process` bugs.

## 1. Rust PTY API lacks a first-class terminal input relay entrypoint

Problem:

- `running-process` exposes `NativePtyProcess`, PTY query handling, and `TerminalInputCore`.
- But the actual Windows PTY terminal-input relay wiring is only exposed in `running-process-py`.
- A Rust consumer using `running-process` directly can easily create a PTY that shows child output but never forwards keyboard input.

Evidence:

- `crates/running-process/src/pty/mod.rs`
- `crates/running-process/src/pty/terminal_input.rs`
- `crates/running-process-py/src/lib.rs` implements `start_terminal_input_relay_py(...)`

Why this belongs here:

- This is an API parity gap between the core Rust crate and the Python binding layer.
- It creates a sharp edge for downstream Rust consumers.

What to add:

- A core-level `start_terminal_input_relay(...)` / `stop_terminal_input_relay(...)` API for `NativePtyProcess`, or
- a higher-level core interactive session type that owns relay lifecycle internally.

## 2. Rust PTY consumers currently have to hand-assemble too many interactive pieces

Problem:

To build a real interactive PTY session with `running-process`, the caller has to know to combine:

- PTY spawn
- output echo
- terminal-input relay
- PTY query replies such as `\x1b[6n`
- exit polling
- interrupt routing
- resize handling
- cleanup and terminal restoration

The Python layer has a higher-level story for this. The Rust core crate does not.

Why this belongs here:

- The current PTY surface is technically powerful but ergonomically unsafe.
- It is easy for a downstream Rust app to build a half-interactive session that appears to work until real keyboard input or Ctrl+C is involved.

What to add:

- A documented core interactive PTY/session facade, similar in spirit to the Python interactive/PTTY wrappers.
- If that is too large, at least a canonical "interactive PTY recipe" helper in Rust.

## 3. Missing Rust-side documentation on when PTY is the wrong transport

Problem:

- PTY is appropriate for interactive TUI/terminal sessions.
- PTY is the wrong default for one-shot prompt execution, batch jobs, or backend calls that should not inherit arbitrary parent stdin.
- The Python-facing docs are clearer here than the Rust core surface.

Why this belongs here:

- Downstream misuse is currently easy.
- The core crate exposes low-level PTY primitives without enough guidance on transport choice and semantic differences from `NativeProcess`.

What to document explicitly:

- PTY merges terminal semantics and behaves like a terminal, not a plain subprocess pipe.
- PTY callers must think about input relay, terminal queries, resize, and interrupt behavior.
- One-shot noninteractive work should generally use `NativeProcess`, not PTY.

## 4. Missing Rust integration tests for the downstream-consumer PTY recipe

Problem:

- The repo has strong PTY coverage, especially through the Python layer.
- But it is still possible for a downstream Rust consumer to misuse `NativePtyProcess` in ways that are not caught by obvious unit tests.

The specific regression shape to guard against is:

- child output is visible
- PTY query replies work
- but normal keyboard input is never relayed, or interrupt/cleanup semantics are incomplete

What to test:

- Rust-side end-to-end interactive PTY launch with terminal input relay enabled
- Windows Ctrl+C behavior
- terminal query/response handling
- child exit cleanup
- follow-up regression asserting that an echoed PTY without relay is not presented as a complete interactive recipe

## 5. Timeout crash + thread-dump watchdog should be standardized across all unit and integration tests

Problem:

- The repo now has a useful per-test timeout watchdog in `tests/conftest.py`.
- That watchdog emits a loud timeout banner, dumps Rust debug traces when available, dumps all Python thread stacks with `faulthandler`, and then force-exits the process.
- The PTY suite also has a module-level `faulthandler.dump_traceback_later(...)` watchdog in `tests/test_pty_support.py` for hangs that can wedge normal teardown.
- This pattern is valuable because it turns "hung forever" failures into actionable crash artifacts.

Why this belongs here:

- The current coverage is uneven.
- Some test entrypoints already benefit from automatic timeout/thread dumps, but this should be a deliberate repo-wide contract for unit and integration tests rather than an ad hoc pattern.
- When a test wedges in native code, subprocess teardown, or a blocked thread, the agent needs crash-time thread dumps by default instead of having to rediscover and re-add this instrumentation.

Agent direction:

- Use the existing `tests/conftest.py` watchdog as the baseline implementation.
- Audit all unit and integration test entrypoints and make sure they run with equivalent timeout crash handling and all-thread stack dumping.
- Reuse shared fixtures/helpers where possible instead of copying bespoke watchdog code into individual files.
- Keep module-local `faulthandler.dump_traceback_later(...)` guards only where a suite has a demonstrated need beyond the shared per-test watchdog.
- Preserve the Rust debug trace dump on timeout when the native module is available.

Definition of done:

- All unit and integration test runs have a consistent timeout crash path that:
  - identifies the timed-out test node id
  - dumps Python thread stacks
  - dumps Rust/native debug traces when available
  - force-exits instead of hanging indefinitely
- CI and local test entrypoints exercise that path through the supported wrappers, not only through one-off manual commands.
- The watchdog behavior is documented clearly enough that future agents extend it instead of re-inventing it.

## Practical takeaway

`running-process` already contains most of the hard pieces.

The issue is that the safest interactive PTY behavior is not exposed cleanly enough at the Rust core layer, so downstream Rust tools are encouraged to reassemble the parts themselves and can get it subtly wrong.
