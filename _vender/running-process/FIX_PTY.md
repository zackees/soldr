# Fix PTY / Interactive Ctrl-C

## Context

`clud` has a Windows interactive Codex launch path that is currently doing a complex dance around Ctrl-C handling:

- on native Windows console, the parent temporarily ignores Ctrl-C
- the child is allowed to receive Ctrl-C directly
- the child may own or mutate terminal state while running an interactive TUI

That arrangement can avoid one class of traceback, but it also creates a bad failure mode:

- the child exits
- the parent survives
- the terminal is left in a broken or "frozen" state

This is not just a `clud` bug. It is a signal that `running-process` needs a first-class, tested story for interactive pseudo-terminal / console subprocesses where:

- the child must remain interactive
- Ctrl-C must be delivered predictably
- terminal state must be restored on exit or interrupt

## Why this matters

The current tests that passed in `clud` were PTY-oriented and still missed the real failure.

Specifically:

- a `winpty`/PTY integration test passed
- the native Windows console path still appears able to leave the terminal unusable

That means the current test surface is too narrow. PTY success is not enough if the normal console path can still break user input after Ctrl-C.

## Use case to support

Interactive subprocess launched from a Python parent, on Windows, with a real TUI:

- parent is a Python CLI
- child is an interactive CLI/TUI
- user presses Ctrl-C during startup or while UI is live
- child may exit because of Ctrl-C
- parent may or may not also receive Ctrl-C
- after cleanup, the terminal must still accept input normally

Examples:

- Codex CLI
- Claude Code
- any curses/full-screen TUI
- tools using ConPTY / winpty / native console modes

## Expected behavior

For an interactive child:

1. Ctrl-C should have a well-defined owner.
2. The process tree cleanup strategy should be explicit.
3. Terminal restoration must happen even on interrupt paths.
4. Parent and child should not fight over console control state.
5. The library should make it easy to choose between:
   - isolated child process group
   - shared-console interactive mode
   - PTY-backed mode

## Requested `running-process` improvements

### 1. Explicit interactive terminal mode

Add a documented mode for interactive children that is not just "captured streaming subprocess" and not just raw `Popen`.

This mode should define:

- whether the child is attached to the real console or a PTY
- who receives Ctrl-C
- how cleanup occurs
- how terminal restoration is guaranteed

### 2. Terminal restoration hooks

If `running-process` owns PTY/console management for an interactive child, it should also own restoration behavior.

At minimum:

- restore terminal mode on normal exit
- restore terminal mode on `KeyboardInterrupt`
- restore terminal mode on forced kill / timeout

### 3. Clear Windows console semantics

Document and test these separately:

- native Windows console
- Git Bash / MSYS / mintty
- winpty / ConPTY-backed PTY

These are not equivalent and should not share assumptions.

## Tests needed

### Unit tests

Add unit tests that model interrupt routing and cleanup decisions without needing the real Codex CLI.

Cover cases like:

- parent ignores Ctrl-C while child is interactive
- child receives Ctrl-C directly
- parent receives `KeyboardInterrupt` and kills child tree
- cleanup path always runs terminal restoration logic

These tests should assert:

- chosen launch flags / mode
- which side handles interrupt
- cleanup callback invocation
- restoration callback invocation

### Integration tests

Add opt-in Windows integration tests for both:

- PTY-backed path
- native console path

The PTY test alone is not sufficient.

The native-console test must attempt to expose the real failure:

1. launch interactive child
2. wait until startup banner or UI marker appears
3. send Ctrl-C
4. wait for child exit
5. verify terminal is still usable

Possible verification strategies:

- launch a follow-up command in the same parent console and confirm input/output still works
- verify console mode flags are restored to their pre-launch values
- verify stdin is not left unreadable or blocked

### Regression tests for partial startup

Add tests for:

- Ctrl-C during startup banner phase
- Ctrl-C after full-screen UI is live
- Ctrl-C during shutdown/cleanup

This matters because console state bugs often differ by phase.

## Concrete failure observed downstream

Downstream project: `clud`

Observed behavior:

- interactive Codex on Windows required special Ctrl-C handling to avoid ugly errors
- after recent changes to let the child receive Ctrl-C more directly, the terminal could become "frozen" again
- existing PTY integration coverage still passed, so the failure likely lives in the native-console branch or in missing restoration logic

This should be treated as a regression target for `running-process` design and tests.

## Goal

`running-process` should provide a real abstraction for interactive subprocess lifecycle on Windows, not just captured output handling.

Success means downstream callers no longer need ad hoc Ctrl-C dances around:

- process groups
- console Ctrl handlers
- PTY allocation differences
- terminal restoration after interrupts
