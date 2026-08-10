## PTY follow-up for `clud` / Codex interactive mode

### What I verified locally on 2026-04-08

Environment:

- Windows 10 build 19045
- `codex` interactive CLI
- `running-process` PTY API: `RunningProcess.pseudo_terminal(...)`

I tested the new PTY path directly with a real Codex launch.

Observed working behavior:

- `RunningProcess.pseudo_terminal(["codex"], text=True)` does start the real interactive Codex UI.
- I was able to read the startup/banner output and confirm the PTY satisfies Codex's TTY requirement.
- This is already much closer to what `clud` needs than its current ad hoc interactive handling.

Concrete evidence from the local run:

- PTY output included the Codex startup banner
- observed marker: `OpenAI Codex (v0.118.0)`

### What does not feel complete yet

#### 1. Interrupt handling still needs a library-level policy

For the real Codex PTY session:

- one raw `write("\\x03")` did not shut it down
- `send_interrupt()` worked, but Codex needed two interrupts before it exited cleanly
- after two `send_interrupt()` calls, `poll()` returned `0` and `wait()` returned `0`

That means `running-process` has the primitive, but not yet the policy.

For downstream callers like `clud`, the library should offer a first-class interrupt escalation helper instead of forcing each caller to reinvent:

- send one PTY interrupt
- wait a grace period
- optionally send a second PTY interrupt
- optionally escalate to terminate/kill

Requested addition:

- a helper like `interrupt_and_wait(...)` or `graceful_interrupt(...)`

Suggested behavior:

- first interrupt
- grace timeout
- optional second interrupt
- terminate/kill fallback
- structured result indicating which step succeeded

#### 2. Interrupt provenance is missing

When Codex exited after PTY interrupts, `wait()` returned `0`.

That may be fine for some consumers, but `clud` also needs to know whether the session ended because:

- the child exited normally
- the parent initiated an interrupt
- a forced terminate/kill happened
- a timeout/idle policy triggered shutdown

Right now there is cleanup callback `reason`, but there is not an obvious structured runtime result for:

- interrupt requested by caller
- interrupt acknowledged by child
- escalated terminate/kill path

Requested addition:

- expose termination provenance/state on the PTY process object or return it from a helper

Examples:

- `exit_reason = "exit" | "interrupt" | "terminate" | "kill" | "timeout"`
- `interrupt_count`
- `interrupted_by_caller: bool`

#### 3. PTY idle detection has the primitives, but not the convenience API

For `clud`, PTY idle detection is implemented outside the library by polling reads and maintaining activity timestamps.

That is possible with the current API, but the library could make this much cleaner.

What already exists and is useful:

- `read(timeout=...)`
- `read_non_blocking()`
- `available()`
- `drain()`
- full chunk history via `output`

What is still missing for the `clud` use case:

- a built-in `wait_for_idle(...)` or `monitor_idle(...)` helper
- activity timestamp support
- a way to plug in an activity predicate so TUI noise does not count as meaningful activity

Requested addition:

- helper API for PTY idle waiting with:
  - idle timeout
  - optional callback/filter deciding whether a chunk counts as activity
  - exit-vs-idle structured result

This would let downstream code stop reimplementing the same monitor loop.

#### 4. `kill()` / `terminate()` should be idempotent after exit

After the real Codex PTY session had already exited, a cleanup `kill()` attempt produced:

- `PermissionError(13, 'Access is denied', None, 5, None)`

Downstream cleanup code often intentionally calls kill/terminate defensively.

Requested addition:

- if the PTY child is already gone, `kill()` and `terminate()` should become safe no-ops
- or at minimum normalize the already-exited case into a library error that is easy to ignore intentionally

#### 5. There should be a documented Codex-grade Windows PTY story

The current PTY API is good, but this exact downstream use case should be documented explicitly:

- launch an interactive full-screen/TUI child on Windows
- read chunked PTY output
- relay Ctrl-C from parent to PTY child
- optionally use double-interrupt semantics
- optionally stop on idle
- restore terminal state / cleanup safely

This should be treated as a first-class scenario, not just a generic PTY demo.

### What I think is already good enough for `clud`

These parts appear ready and useful:

- explicit PTY-specific API instead of overloading the pipe API
- chunk-oriented reads
- `expect(...)`
- `resize(...)`
- `send_interrupt()`
- restoration and cleanup callbacks
- real Windows Codex startup works under PTY

So the new PTY implementation does appear to solve the biggest `clud` issue:

- Codex gets a real terminal-like environment and can launch interactively

### What `clud` would still need from `running-process`

Minimum missing features I would want before switching `clud` over fully:

1. A library-owned interrupt escalation helper for PTY children.
2. Structured termination provenance instead of only raw exit code.
3. A PTY idle-monitor helper or at least activity timestamp support.
4. Idempotent post-exit `kill()` / `terminate()`.

### Proposed tests to add here

Add targeted tests for the missing behavior:

1. PTY interrupt escalation test

- child ignores first interrupt or stays alive
- second interrupt exits
- helper reports which step succeeded

2. PTY idle helper test

- child emits redraw noise
- filter ignores non-meaningful chunks
- helper returns idle only after meaningful inactivity

3. PTY post-exit cleanup idempotency test

- child exits
- `kill()` and `terminate()` do not raise noisy OS errors

4. Real opt-in Codex PTY integration test

- launch `codex`
- wait for `OpenAI Codex`
- send interrupt once or twice as required
- verify clean exit path

### Bottom line

The new PTY work is directionally correct and does appear viable for the `clud` Codex use case.

The remaining gap is not "can PTY launch Codex?".

That part works.

The remaining gap is:

- library-owned interrupt semantics
- library-owned idle-monitor semantics
- cleaner post-exit lifecycle semantics

Those are the pieces that would let downstream callers stop writing fragile PTY control code themselves.
