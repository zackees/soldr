# Refactor Directive: `RunningProcess` as a Thin Proxy over `NativeProcess`

## Summary

The current architecture still reflects the old Python-first implementation model.

That model made sense when Python had to do all of the following itself:

- coordinate subprocess lifecycle
- poll for exit
- poll for output
- manage PTY readers
- infer EOF
- emulate eventing with loop-and-sleep patterns
- reconcile platform differences in user space

That is no longer the right architecture.

Now that the package has a Rust native layer, the Python `RunningProcess` object should stop acting like a process manager and instead become a thin API facade over a single native controller: `NativeProcess`.

The directive is:

- move the real lifecycle and synchronization logic into Rust
- unify native subprocess and native PTY handling under one native abstraction
- make Python mostly declarative and API-oriented
- eliminate Python polling loops and timing-sensitive control paths wherever possible

The end state is:

- Python exposes a nice cross-platform API
- Rust owns the real state machine
- Rust dispatches internally to either a native subprocess backend or a native PTY backend
- waits, output delivery, exit detection, teardown, and synchronization happen natively

## Problem Statement

### The current split is upside down

Today, `RunningProcess` is nominally an abstraction layer, but in practice it still performs real orchestration work.

Python is still responsible for too much:

- wait loops
- timeout loops
- incremental output polling
- expect/wait condition polling
- EOF inference
- PTY reader coordination
- join timing
- close-after-exit fallback behavior

This creates several structural problems.

### 1. Python remains in the hot path

Even though Rust now owns real OS handles, Python still often decides:

- when to check exit
- when to sleep
- when to wake
- when to force close
- when to declare EOF

That means performance is still shaped by:

- GIL scheduling
- Python thread coordination
- polling granularity
- condition-check timing in Python

This is fundamentally the wrong layer for those responsibilities.

### 2. There are too many duplicated state machines

There is no single authoritative process lifecycle model.

Instead, pieces of the lifecycle are spread across:

- `RunningProcess`
- `PseudoTerminalProcess`
- Python reader thread logic
- `NativeRunningProcess`
- `NativePtyProcess`

This creates drift in behavior and subtle bugs around:

- exit detection
- late output
- reader shutdown
- timeout handling
- EOF handling
- kill vs close vs terminate ordering

### 3. Python polling was a workaround, not a design target

Loop-sleep patterns in Python exist because the package historically had to work around limited coordination.

Examples include:

- polling for child exit
- polling for output visibility
- polling in `wait_for`
- polling in idle detection
- polling in PTY reader coordination

Those loops were originally compensating for the lack of a native concurrency and synchronization layer.

Now that Rust exists, keeping those loops is mostly retaining technical debt.

### 4. Platform-neutral API does not require Python-owned lifecycle logic

A clean Python API and a Python-owned state machine are not the same thing.

The job of Python should be:

- ergonomic API surface
- argument normalization
- optional formatting and convenience
- exception mapping
- object model consistency

The job of Rust should be:

- owning handles
- owning process lifecycle
- owning wait/event coordination
- owning I/O delivery
- owning platform-specific behavior

## Target Architecture

## Core principle

There should be one native process controller with one native state machine:

- `NativeProcess`

It should be the only object that truly owns process execution semantics.

Internally, `NativeProcess` should wrap one of two native backends:

- `NativeSubprocessBackend`
- `NativePtyBackend`

Python should not need to care which backend is active beyond feature differences like PTY-specific operations.

## High-level shape

Python side:

- `RunningProcess`
  - thin proxy
  - public API
  - argument normalization
  - exception translation
  - small convenience helpers

Rust side:

- `NativeProcess`
  - unified lifecycle controller
  - authoritative state machine
  - public native API for Python
- `SubprocessBackend`
  - pipes / normal subprocess mode
- `PtyBackend`
  - PTY mode

Conceptually:

```text
RunningProcess (Python facade)
        |
        v
NativeProcess (Rust controller / state machine)
        |
        +-- SubprocessBackend
        |
        +-- PtyBackend
```

## Desired ownership model

### Python owns

- API ergonomics
- parameter coercion
- convenience wrappers
- user-facing exceptions
- optional high-level helpers

### Rust owns

- process state
- process handles
- pipes / PTY handles
- child exit detection
- output buffering
- output signaling
- EOF signaling
- wait semantics
- timeout semantics
- idle / event signaling
- kill / terminate / close ordering
- reader thread management if threads are needed

The important part is not "more code in Rust".

The important part is:

- one owner of the lifecycle
- one owner of concurrency
- one owner of timing-sensitive behavior

## What `NativeProcess` should look like

## Required behavioral surface

`NativeProcess` should expose one coherent native API to Python.

It should support operations like:

- `start()`
- `poll()`
- `wait(timeout=None)`
- `terminate()`
- `kill()`
- `close()`
- `send_interrupt()`
- `write(data)`
- `read(timeout=None)`
- `read_non_blocking()`
- `drain()`
- `available()`
- `output()`
- `output_since(offset)`
- `history_bytes()`
- `clear_history()`
- `checkpoint()`
- `wait_for(...)`
- `wait_for_idle(...)`
- `expect(...)`
- `resize(...)` for PTY mode

Not every backend needs every operation, but `NativeProcess` should define the contract and route appropriately.

## Internal responsibilities

`NativeProcess` should:

- own a single explicit state machine
- expose synchronization-safe operations to Python
- hide backend differences behind consistent semantics

Example internal states:

- `Created`
- `Starting`
- `Running`
- `ExitObserved`
- `DrainingOutput`
- `Closed`
- `Failed`

These do not need to be the exact names, but the key is explicit state transitions rather than distributed implicit behavior.

## Required guarantees

### 1. Exit must be authoritative

Once native code observes exit, that fact should be owned and published centrally.

Python should not "discover" exit via retry loops.

### 2. EOF must be authoritative

EOF should come from the native layer as a real state transition, not from Python inference.

### 3. Output visibility must be coordinated with lifecycle

The native layer should guarantee a coherent relationship between:

- process exit
- buffered output availability
- reader completion
- EOF publication

That means Python should not need fallback logic like:

- "if exited, join thread, maybe force close, then check output again"

### 4. Timeout behavior must be native

Timeout waiting should happen in Rust where possible.

That allows:

- proper blocking primitives
- proper condvars/events
- lower wakeup latency
- no Python loop-sleep overhead

## What must move out of Python

## 1. Wait loops

These should move to native code:

- `wait()`
- PTY wait loops
- exit polling loops
- timeout loops
- `_wait_until_exit()`
- native-exit watcher loops

Python should call one native wait operation and get back:

- completed
- timed out
- interrupted
- exited abnormally

## 2. PTY reader coordination

The native layer should fully own:

- PTY read lifecycle
- EOF observation
- reader shutdown
- post-exit drain behavior

Python should not own the logic for:

- joining reader threads
- forcing close after exit
- timing grace joins

If a background reader thread is necessary, it should be a native implementation detail.

## 3. `expect()` orchestration

At minimum, the core waiting and output coordination should move into Rust.

Potential levels:

### Good intermediate state

Rust owns:

- blocking wait for more output / exit / EOF
- output buffering
- checkpoint offsets

Python still owns:

- regex matching
- action callbacks

### Better end state

Rust also owns:

- substring matching
- regex matching
- checkpoint-relative search
- timeout enforcement

Python only receives:

- matched / not matched
- span
- groups
- output slices

The second option is better for performance and correctness, but the first is still a major improvement.

## 4. Idle detection orchestration

Idle detection is exactly the kind of timing-sensitive logic that should live natively.

Rust should own:

- sample timing
- process metrics sampling
- PTY activity tracking
- stability windows
- idle timeout decisions

Python callbacks may still exist, but the scheduler and state transitions should be native.

## 5. Output buffering and history

This should be centered natively.

The native layer should own:

- combined output history
- stdout/stderr buffers in subprocess mode
- PTY output history
- offsets/checkpoints
- output availability signaling

Python should not need to constantly reassemble buffers or infer incremental progress via polling.

## Why this is faster

## 1. No GIL-owned lifecycle loops

The biggest gain is not raw CPU speed alone.

It is removing Python from timing-sensitive coordination.

That means:

- fewer GIL handoffs
- fewer Python wakeups
- fewer sleeps
- fewer races between Python threads and native state

## 2. Blocking waits become real waits

Instead of:

- Python loop
- `poll()`
- `sleep(0.01)`
- re-check

You get:

- native wait on condvar/event/handle
- wake exactly when state changes

That is both faster and more correct.

## 3. One state machine means less duplicated work

Today multiple layers repeatedly ask:

- has the child exited?
- is there output?
- is the stream closed?
- should I join now?

With one native controller, those answers are centralized.

## 4. Better PTY handling

PTYs are timing-sensitive and platform-specific.

Those are precisely the cases where Python is the wrong orchestration layer.

Native code can:

- drop handles in the correct order
- wait on platform primitives directly
- signal EOF cleanly
- unblock readers immediately

## Advantages of `NativeProcess`

`NativeProcess` is not just a cleanup of the API boundary. It creates a better execution model.

The main advantages are:

### 1. One authoritative owner of truth

Today, truth is split across:

- Python object state
- Python reader thread state
- native process state
- native PTY handle state

With `NativeProcess`, there is one authoritative source for:

- whether the child is alive
- whether output is pending
- whether EOF has occurred
- whether the process is closing
- whether final output has been drained
- whether a timeout has fired

That reduces race conditions and removes guesswork from the Python layer.

### 2. Native waiting semantics

Instead of emulating asynchronous state changes with Python polling loops, `NativeProcess` can block efficiently on real synchronization primitives and wake exactly when needed.

That gives:

- lower CPU burn
- lower latency
- less wakeup jitter
- fewer redundant state checks

### 3. Backend-specific optimization without API leakage

`NativeProcess` can optimize PTY and subprocess behavior differently internally while still presenting one consistent Python interface.

For example:

- subprocess mode can use pipe readers and direct process wait handles
- PTY mode can use PTY readers and PTY-specific close/EOF semantics

Python does not need to know any of that.

### 4. Cleaner platform-specific behavior

Windows and POSIX have genuinely different process and PTY semantics.

`NativeProcess` can encapsulate:

- Windows process handles
- Windows ConPTY semantics
- Windows job object behavior
- POSIX process groups
- POSIX PTY EOF behavior
- signal differences

That keeps Python from turning into a platform-reconciliation layer.

### 5. Better failure semantics

A native controller can explicitly model:

- start failure
- reader failure
- backend failure
- close failure
- partial output delivery
- callback failure

Rather than letting these emerge indirectly through timing or Python polling behavior.

### 6. Easier future extension

Once a real native process controller exists, new features become easier:

- native event subscriptions
- richer diagnostics
- zero-copy buffer sharing
- native matcher engines
- cancellation tokens
- native idle detectors
- better async integration later if desired

## Native communication and synchronization primitives

`NativeProcess` will need internal coordination primitives to manage communication with:

- `NativeSubprocessBackend`
- `NativePtyBackend`
- reader threads
- optional watcher threads
- callback dispatch paths
- buffer consumers

The point is not to choose one primitive for everything.

The right design is a small set of primitives, each used for the job it is good at.

## 1. Atomics

Atomics are ideal for simple shared state flags and counters that are read often and changed cheaply.

Good uses:

- `is_running`
- `exit_observed`
- `reader_closed`
- `close_requested`
- `interrupt_requested`
- `buffer_version`
- monotonic event counters
- cancellation flags

Benefits:

- extremely cheap reads
- good for hot-path status checks
- avoids mutex contention for simple state

Limits:

- not good for complex multi-field consistency
- not sufficient when a thread must block until something changes

Atomics should be used for:

- cheap visibility
- fast-path checks
- wake condition prechecks

Not for:

- full lifecycle ownership by themselves

## 2. Mutexes

Mutexes are appropriate for structured shared state that must change coherently.

Good uses:

- the main process state machine struct
- buffered output data structures
- result objects
- callback registration tables
- backend-owned handle groups

Examples of data protected by a mutex:

- current process state
- return code
- pending output spans
- EOF state
- backend-specific teardown state

Benefits:

- easy correctness model
- supports coherent state transitions
- necessary for complex shared structs

Limits:

- too expensive for constant hot-path polling if misused
- should not be held across blocking operations unless designed very carefully

## 3. Condition variables

Condition variables are one of the most important primitives for the refactor.

They should replace many Python loop-sleep patterns.

Good uses:

- waiting for new output to arrive
- waiting for EOF
- waiting for process exit
- waiting for state transitions like `ExitObserved -> DrainingOutput -> Closed`
- waking readers/waiters when buffer state changes

Example pattern:

- mutex protects process state and output buffer metadata
- condvar wakes any waiter when:
  - output appended
  - EOF observed
  - exit observed
  - close requested

Benefits:

- exact wakeup on meaningful events
- ideal replacement for repeated `poll()+sleep`
- works well with timeout waits

This should be the default primitive for:

- `wait()`
- `read(timeout=...)`
- `wait_for_output`
- `wait_for_exit`

## 4. Queues / channels

A queue or channel is appropriate when one side produces discrete events or work items and another consumes them.

Possible uses:

- output chunks from reader thread to buffer owner
- lifecycle events from backend to controller
- callback work dispatch
- diagnostic/event stream emission

Examples of queued event types:

- `OutputChunk(bytes)`
- `ExitObserved(code)`
- `EofObserved`
- `ReaderFailed(error)`
- `CloseRequested`
- `InterruptRequested`

Benefits:

- clear producer/consumer boundaries
- good for decoupling reader threads from the controller
- works well for event-driven internal architectures

Limits:

- a queue alone does not replace shared state
- still needs state ownership and synchronization around it

A good architecture may use:

- queue for discrete events
- mutex + condvar for authoritative shared state

## 5. Threads

Rust threads are appropriate where true background work is needed.

Potential native threads:

- PTY reader thread
- subprocess stdout reader thread
- subprocess stderr reader thread
- optional metrics sampler
- optional callback dispatcher

Important principle:

- threads should be an implementation detail of `NativeProcess`, not a Python coordination burden

Benefits:

- real concurrency without the Python GIL
- lower-latency I/O consumption
- clearer backend-specific work loops

Limits:

- thread count should stay intentional
- do not create watcher threads when a blocking handle wait or event primitive can do the job better

The design should prefer:

- blocking handle waits or condvars first
- native threads only where genuinely needed

## 6. Semaphores

Semaphores are less likely to be the core primitive here, but they may still be useful.

Possible uses:

- bounded work queues
- limiting callback dispatcher concurrency
- backpressure on chunk-processing pipelines

They are not the first primitive I would reach for in the core lifecycle design, but they may be useful as supporting infrastructure.

## 7. One-shot events / notify primitives

Depending on the Rust ecosystem choices, one-shot wakeup mechanisms can be useful for:

- close requested
- shutdown requested
- cancellation
- backend teardown completion

These are useful where a state change happens once and waiters should unblock quickly.

Conceptually these are very good for:

- `close()`
- `kill()`
- canceling waiters
- one-time exit publication

## 8. Callback hooks

Callbacks should exist only where they add real user-facing value, and they should be treated carefully.

There are two kinds:

### Internal callbacks

These are native implementation details.

Examples:

- backend notifying controller about exit
- reader notifying buffer manager about chunk arrival

These are fine as internal architectural tools.

### Python callbacks

These are much more expensive and risky because they require:

- reentering Python
- taking the GIL
- handling exceptions carefully
- deciding thread affinity

Examples:

- `wait_for` callbacks
- `idle_reached` callbacks
- `on_callback` actions

These should be:

- optional
- minimized in the hot path
- invoked from well-defined boundaries

The native layer should not depend on Python callbacks for core correctness.

Callbacks may influence policy, but must not be required for lifecycle progress.

## 9. Read-write locks

These are probably not necessary for the main design.

Most of the important data here is write-heavy enough or transition-heavy enough that a simple mutex is usually the better tool.

They may be useful for:

- infrequently mutated configuration shared across many readers

But they should not be the default.

## Recommended internal model

The most likely good design is a hybrid:

### Core shared state

- `Mutex<ProcessState>`
- `Condvar`
- a few `AtomicBool` / `AtomicU64` flags for fast-path status

### Output ingestion

Either:

- reader threads append directly into a mutex-protected buffer and notify the condvar

Or:

- reader threads send discrete events/chunks into a channel
- controller thread/process state owner applies them to shared state

### Exit publication

- backend detects exit
- updates native shared state under mutex
- stores return code
- marks exit flag atomically if useful
- notifies all waiters via condvar

### Close / kill / terminate

- request state transition under mutex
- execute backend-specific teardown
- publish final state
- notify condvar

### Python-facing waits

Python calls:

- `wait`
- `read`
- `expect`
- `wait_for`
- `wait_for_idle`

Native code:

- blocks on condvar / backend wait primitive
- wakes on output, exit, EOF, timeout, or cancellation
- returns structured results

## Communication model between `NativeProcess` and backends

There are several viable designs.

## Option A: Direct trait/object dispatch with shared state

`NativeProcess` owns:

- shared state
- synchronization

Backend implements:

- low-level OS operations

This is probably the cleanest baseline.

Example conceptual interface:

```rust
trait ProcessBackend {
    fn start(&mut self) -> Result<()>;
    fn poll_exit(&mut self) -> Result<Option<i32>>;
    fn terminate(&mut self) -> Result<()>;
    fn kill(&mut self) -> Result<()>;
    fn close(&mut self) -> Result<()>;
    fn write(&mut self, data: &[u8]) -> Result<()>;
    fn resize(&mut self, rows: u16, cols: u16) -> Result<()>;
}
```

Reader threads or backend helpers then feed state changes back into `NativeProcess`.

## Option B: Backend emits events into a channel

Backends push events like:

- `OutputChunk`
- `ExitObserved`
- `ReaderClosed`
- `Error`

`NativeProcess` consumes those events and updates the authoritative state.

This can be very clean if the system becomes more event-driven.

## Option C: Backend owns more logic, `NativeProcess` coordinates at a higher level

This is viable, but risks recreating split ownership.

It is acceptable only if:

- `NativeProcess` still remains the authoritative public controller
- backend-local logic is clearly subordinate

The refactor should avoid drifting back into:

- Python state machine
- native wrapper state machine
- backend-specific hidden state machine

There should still be one clear owner of lifecycle truth.

## Primitive recommendations by concern

### For process lifecycle state

Use:

- mutex
- condvar
- atomics for quick flags

### For output buffering

Use:

- mutex for buffer data
- condvar for "new output available"

Optional:

- queue/channel if readers are decoupled from the state owner

### For PTY reader management

Use:

- native thread if needed
- close/cancel flag via atomic
- condvar notification on EOF/output/close

### For exit detection

Use:

- backend wait handle or platform primitive first
- mutex-protected state transition
- condvar broadcast on exit

### For callback orchestration

Use:

- queue or dispatcher thread if callbacks must be serialized
- explicit Python reentry boundaries

### For timeout handling

Use:

- timed condvar waits
- timed backend wait primitives

Not:

- Python sleep loops

## Rules for primitive selection

1. Do not use atomics where a multi-field state transition must be coherent.
2. Do not use polling where a condvar or handle wait can block correctly.
3. Do not use Python callbacks as the primary progress mechanism.
4. Do not hold mutexes across long OS waits unless absolutely necessary and carefully designed.
5. Use queues/channels for discrete event flow, not as a substitute for authoritative shared state.
6. Use threads only where blocking readers or backend-specific work genuinely require them.
7. Prefer exact wakeup semantics over periodic wakeup semantics.

## What Python `RunningProcess` should become

## Design goal

`RunningProcess` should feel rich, but be mechanically thin.

That means Python can still offer:

- `wait()`
- `expect()`
- `wait_for()`
- `wait_for_idle()`
- `interrupt_and_wait()`
- `output`
- `stdout`
- `stderr`
- `checkpoint()`

But each of those should mostly be:

- argument translation
- one native method call
- result translation

Not:

- a control loop
- a scheduler
- a state machine

## Example philosophy

Bad:

```text
Python decides the process lifecycle and asks native code for small pieces of information.
```

Good:

```text
Python requests an operation from native code, and native code executes the lifecycle transition and returns a result.
```

## Proposed refactor stages

## Stage 1: Introduce `NativeProcess` as the unified facade

Build a single native class exposed to Python:

- `NativeProcess`

It should internally select:

- subprocess backend
- PTY backend

Keep behavior close to current semantics first.

Goal:

- unify ownership before changing all semantics

## Stage 2: Move all wait/exit/teardown logic into `NativeProcess`

Eliminate Python-owned:

- wait polling
- exit watcher threads
- PTY join coordination
- timeout coordination

Goal:

- Python no longer owns lifecycle timing

## Stage 3: Move output buffering and checkpointing fully native

Unify:

- read
- drain
- output
- output_since
- history_bytes

Goal:

- Python stops managing buffer evolution as a logic concern

## Stage 4: Move `wait_for` orchestration native

At first:

- native waits for output/exit/timeout
- Python still does pattern callbacks if necessary

Then later:

- native owns matching too

Goal:

- remove Python polling for condition waits

## Stage 5: Move idle detection fully native

Goal:

- native timers
- native metric sampling
- native stability windows
- Python callbacks only as optional policy hooks

## Stage 6: Simplify Python API layer aggressively

Once the native layer is authoritative:

- delete Python fallback orchestration
- delete Python polling loops
- delete Python lifecycle heuristics
- keep only API translation and convenience

## Non-goals

This directive does not require:

- removing the Python API richness
- exposing raw Rust complexity to users
- abandoning convenience helpers

It also does not mean:

- every callback must become native immediately
- every regex must be reimplemented in Rust on day one

The point is to move authority first, not necessarily every feature at once.

## Risks

## 1. Callback semantics are harder when logic moves native

Anything involving Python callbacks from native waits requires careful GIL boundaries and explicit control.

That means callback-heavy features may need staged migration.

## 2. Regex and matching semantics can drift

If matching moves native, care is needed to preserve:

- Python regex compatibility where promised
- checkpoint semantics
- callback ordering

## 3. Backend unification can over-generalize

Subprocess mode and PTY mode are not identical.

`NativeProcess` should unify lifecycle and API shape, but it should not erase real backend differences where they matter.

## 4. The first unified layer may be more code before it becomes less code

That is acceptable.

The target is lower long-term complexity and higher correctness, not fewer lines on day one.

## Architectural rules for the refactor

1. Python must not own polling loops for core lifecycle operations.
2. Python must not infer EOF that native code can know directly.
3. Native code must own the authoritative process state machine.
4. PTY reader lifecycle must be fully native-owned.
5. Timeouts for core waits should be enforced natively.
6. Buffer ownership should be native-first.
7. Python should translate API and exceptions, not orchestrate process state.

## Concrete vision

The ideal future object model looks like this:

```python
proc = RunningProcess(...)
proc.start()
proc.wait(timeout=5)
proc.expect("ready>", timeout=2)
proc.wait_for(...)
proc.wait_for_idle(...)
```

But under the hood, all of those become thin wrappers over something like:

```python
self._native = NativeProcess(...)
```

And that native object is the real engine:

- real state
- real handles
- real synchronization
- real output lifecycle
- real backend dispatch

That is the right architecture for a package that now has Rust.

## Final Directive

`RunningProcess` should no longer be a Python process manager with native helpers.

It should become a Python API facade over a single native controller.

The native controller should:

- own the lifecycle
- own the synchronization
- own the buffering
- own the timing
- own the platform behavior

And Python should:

- expose the interface
- normalize arguments
- convert results to ergonomic objects

That is the correct long-term shape for maximum performance, correctness, and maintainability.

## Migration Action Plan

This refactor should not be attempted as a single rewrite.

It should be executed as a staged migration where:

- behavior remains testable at every step
- old and new paths can coexist temporarily where needed
- every successful milestone ends in a commit
- rollback is always available

The goal is to continuously move ownership from Python `RunningProcess` into native `NativeProcess` without losing correctness.

## Migration principles

1. Move authority before deleting compatibility.
2. Keep one narrow migration target per phase.
3. Do not move multiple timing-sensitive systems at once unless they share the same native foundation.
4. Make every phase observable and benchmarkable.
5. Commit after every stable checkpoint.

## Backup and rollback strategy

Before each phase:

- create a branch for the phase
- keep the previous phase commit as a known-good rollback point
- do not stack unrelated refactors into the same commit

Recommended branch/backup strategy:

- `main` stays releasable
- feature branch for the entire migration:
  - example: `native-process-refactor`
- optional sub-branches for dangerous phases:
  - `native-process-phase-1`
  - `native-process-phase-2`

Rollback policy:

- if a phase destabilizes tests, reset that branch to the last successful commit
- do not carry broken intermediate state forward
- if a phase partially lands but semantics are unclear, revert the phase commit cleanly instead of patching blindly on top

Artifact backup policy:

- save benchmark results for each phase
- save representative trace logs before and after major changes
- save test runtime summaries for PTY and subprocess suites

Useful artifacts to keep:

- targeted pytest output
- benchmark numbers
- trace summaries
- notes on behavioral changes

## Commit discipline

Every successful phase ends with:

1. tests passing for the targeted scope
2. known performance characteristics recorded
3. one focused commit

Commit rule:

- one phase, one commit

If a phase is large:

- split into 2-3 coherent commits, but only if each is independently valid and testable

Commit message style:

- `Introduce NativeProcess facade`
- `Move exit and wait lifecycle into NativeProcess`
- `Move PTY reader lifecycle into NativeProcess`
- `Move output buffering into NativeProcess`
- `Move wait_for orchestration into NativeProcess`
- `Move idle detection into NativeProcess`
- `Simplify Python RunningProcess facade`

## Phase-by-phase implementation plan

## Phase 0: Establish baseline and freeze current behavior

### Goal

Create a measurable baseline before moving architecture.

### Work

- identify the critical public behaviors of:
  - `RunningProcess`
  - `PseudoTerminalProcess`
  - `InteractiveProcess`
- identify the hot tests:
  - PTY tests
  - wait/expect tests
  - idle-detection tests
  - subprocess tests
- record baseline runtime for:
  - `tests/test_pty_support.py`
  - subprocess-related test files
  - any integration suite that covers wait/expect/idle
- preserve the current trace and benchmark tools for temporary use during migration

### Tests

- run targeted PTY tests
- run targeted subprocess tests
- run any existing end-to-end tests covering process lifecycle

### Exit criteria

- baseline behavior documented
- baseline runtime documented
- baseline branch/commit available

### Commit

- `Document NativeProcess migration baseline`

## Phase 1: Introduce `NativeProcess` as a facade only

### Goal

Create a single native object exposed to Python without changing most behavior yet.

### Work

- add a new native class:
  - `NativeProcess`
- make it internally own either:
  - `NativeSubprocessBackend`
  - `NativePtyBackend`
- keep Python behavior mostly unchanged, but route object construction through `NativeProcess`

At this stage, `NativeProcess` may still delegate substantial behavior to existing native classes. That is acceptable.

The point is to create the architectural seam.

### Tests

- object construction tests
- subprocess launch tests
- PTY launch tests
- compatibility tests for existing Python API

### Exit criteria

- Python can instantiate and use `NativeProcess`
- backend selection is correct
- no user-visible API breakage

### Commit

- `Introduce NativeProcess unified native facade`

## Phase 2: Move exit, wait, and teardown lifecycle into `NativeProcess`

### Goal

Make `NativeProcess` the authoritative owner of:

- process state
- exit observation
- timeout waiting
- close/kill/terminate sequencing

### Work

- define the native process state machine
- move the canonical state fields into `NativeProcess`
- make `wait()` native-first
- make `poll()` native-authoritative
- make `close()`, `kill()`, and `terminate()` fully native-coordinated
- ensure subprocess and PTY backends both publish lifecycle events into the same state model

### Tests

- process exit tests
- timeout tests
- kill/terminate tests
- abnormal exit tests
- keyboard interrupt / signal tests
- PTY teardown tests

### Performance checks

- compare `wait()` latency before/after
- compare PTY teardown latency before/after

### Exit criteria

- Python no longer owns core wait loops for exit
- teardown ordering is native-owned
- PTY and subprocess lifecycle are consistent through `NativeProcess`

### Commit

- `Move lifecycle and teardown into NativeProcess`

## Phase 3: Move PTY reader lifecycle into `NativeProcess`

### Goal

Remove Python ownership of PTY reader coordination.

### Work

- make PTY reader threads native if threads are still required
- move PTY EOF publication into native state
- move reader shutdown sequencing into `NativeProcess`
- remove Python-side reader join heuristics
- remove Python-side forced close-after-exit orchestration where possible

### Tests

- PTY interactive output tests
- PTY EOF tests
- PTY final-output preservation tests
- kill/terminate-after-exit tests
- tests that previously hit reader thread join timeouts

### Performance checks

- measure exit-to-reader-closed latency
- measure PTY teardown wall time

### Exit criteria

- Python no longer manages PTY reader lifecycle correctness
- native layer owns PTY output completion and EOF

### Commit

- `Move PTY reader lifecycle into NativeProcess`

## Phase 4: Move output buffering and checkpoint state into `NativeProcess`

### Goal

Make output state authoritative and native-first.

### Work

- unify buffer ownership inside native code
- define one native output history model
- support:
  - `output()`
  - `output_since(offset)`
  - `history_bytes()`
  - `clear_history()`
  - checkpoint creation
- ensure subprocess and PTY modes expose the same conceptual buffer interface where appropriate

### Tests

- output accumulation tests
- checkpoint tests
- drain/read tests
- mixed-output tests for subprocess mode
- PTY output history tests

### Performance checks

- compare buffer update overhead
- compare history slicing costs

### Exit criteria

- Python no longer reconstructs meaningful output state
- checkpoint semantics are native-owned

### Commit

- `Move output buffering into NativeProcess`

## Phase 5: Move `wait_for` orchestration into `NativeProcess`

### Goal

Eliminate Python polling for condition waits.

### Work

- move the core wait-for loop to native code
- native waits on:
  - output available
  - exit observed
  - EOF observed
  - timeout
- return structured results to Python

Initial acceptable split:

- native owns waiting and output progression
- Python still owns some matcher/callback policy

Preferred later state:

- native owns matching and timeout enforcement too

### Tests

- `wait_for` expect tests
- `wait_for` callback tests
- timeout tests
- process-exit-before-match tests
- EOF-before-match tests

### Performance checks

- compare `wait_for` runtime
- compare loop wake count and CPU usage

### Exit criteria

- no Python loop-sleep in core `wait_for` orchestration
- condition waits are driven by native events

### Commit

- `Move wait_for orchestration into NativeProcess`

## Phase 6: Move `expect` matching and EOF handling deeper into native

### Goal

Remove Python polling and EOF inference from `expect()`.

### Work

- make native code own:
  - output wait
  - EOF publication
  - timeout progression
- optionally move:
  - substring matching
  - regex matching
  - checkpoint-relative search

If regex migration is too large initially:

- keep Python regex evaluation but make native provide deterministic wakeups and buffer deltas

### Tests

- EOF tests
- timeout tests
- chained expect tests
- next-expect tests
- NOT/suppress match tests
- constructor-time expect tests

### Performance checks

- compare expect latency
- verify no false full-timeout waits on EOF

### Exit criteria

- `expect()` no longer depends on Python polling for correctness
- EOF behavior is native-authoritative

### Commit

- `Move expect lifecycle into NativeProcess`

## Phase 7: Move idle detection into `NativeProcess`

### Goal

Make idle timing and sampling native-owned.

### Work

- move idle timing windows into native
- move PTY activity accounting into native
- move process metrics sampling into native
- make idle waits use native timed waits and wakeups
- keep Python callbacks only as optional policy hooks if necessary

### Tests

- idle timeout tests
- idle stability window tests
- idle callback tests
- mixed process/PTY idle detection tests

### Performance checks

- compare idle wait CPU usage
- compare idle decision timing accuracy

### Exit criteria

- idle scheduling and sampling are native-owned
- Python no longer polls to implement idle detection

### Commit

- `Move idle detection into NativeProcess`

## Phase 8: Simplify Python `RunningProcess`

### Goal

Delete migrated orchestration logic from Python and leave only the facade.

### Work

- remove Python lifecycle state that duplicates native state
- remove Python wait loops
- remove Python output coordination heuristics
- reduce Python classes to:
  - API normalization
  - convenience methods
  - exception translation
  - optional callback wrappers

### Tests

- full PTY test suite
- full subprocess-related test suite
- API compatibility tests
- regression tests for public behavior

### Exit criteria

- Python layer is thin
- NativeProcess is authoritative
- duplicated state machines are gone

### Commit

- `Simplify RunningProcess into a thin NativeProcess facade`

## Testing plan

Testing must happen at three levels throughout the migration.

## 1. Behavioral regression tests

These confirm that public semantics remain correct.

Target categories:

- subprocess launch and wait
- PTY launch and wait
- output capture
- interactive I/O
- expect semantics
- timeout semantics
- idle semantics
- kill / terminate / interrupt behavior
- platform-specific signal/process-group behavior

## 2. Lifecycle-specific tests

These target the native state machine directly where possible.

Ideal new tests:

- native exit publication tests
- native EOF publication tests
- reader lifecycle tests
- close-after-exit tests
- final-output-drain tests
- timeout transition tests
- state-transition correctness tests

These should become more important as logic migrates downward.

## 3. Performance regression tests

Every major phase should measure:

- PTY file runtime
- key subprocess file runtime
- exit latency
- EOF latency
- idle wait CPU behavior
- repeated wait/expect latency

At least one benchmark or runtime summary should be recorded per phase.

## How to test each phase

Minimum per-phase testing checklist:

1. `py_compile` / Rust build succeeds
2. targeted PTY tests pass
3. targeted subprocess tests pass
4. a representative timing benchmark is recorded
5. if semantics changed, add/update tests before commit

Recommended command categories:

- build/install cycle
- targeted PTY pytest file
- targeted subprocess pytest file
- any integration tests covering user-facing API

## When to add tests

Add tests immediately when:

- a new lifecycle guarantee is introduced
- a bug is fixed due to ambiguity in ownership
- a timeout/EOF/exit behavior changes

Do not postpone those tests to the end.

## Success criteria for the migration

The migration is successful when:

- `RunningProcess` is primarily facade code
- there is one authoritative native process controller
- PTY and subprocess backends share one lifecycle model
- Python polling loops for core lifecycle paths are removed
- EOF, exit, and final output ordering are native-authoritative
- `wait`, `expect`, `wait_for`, and `wait_for_idle` are native-driven
- performance is measurably better
- correctness is at least as good as the current implementation

## Final working rule

After each successful phase:

1. run the targeted tests
2. record the before/after behavior and timing
3. make one focused commit
4. do not start the next phase until the current phase is stable

This must be treated as a controlled migration, not a rewrite sprint.

## Build and Deployment Steps for Each Test Cycle

The migration will touch both:

- Python code
- Rust native code

So every meaningful phase needs a repeatable build/deploy/test cycle.

The key rule is:

- do not trust source edits alone
- always rebuild and reinstall the wheel before drawing conclusions from tests

This matters because the test runner may be importing the installed package from `.venv\\Lib\\site-packages` rather than executing directly from the source tree.

## Standard build/test loop

For each implementation phase:

1. edit source
2. run Python syntax checks for touched Python files
3. rebuild the Rust/Python wheel
4. reinstall the built wheel into the active environment
5. run targeted tests
6. run broader regression tests if the phase is stable
7. record results
8. commit if successful

## Python syntax validation

Before building the wheel, validate touched Python files.

Example:

```powershell
.\.venv\Scripts\python.exe -m py_compile src\running_process\pty.py src\running_process\running_process.py
```

This catches:

- syntax errors
- indentation mistakes
- accidental partial edits

It is a cheap gate and should happen before spending time rebuilding.

## Rust/Python build step

The repo already uses:

- `build.py`

That script wraps the wheel build and dev reinstall flow.

Standard command:

```powershell
.\.venv\Scripts\python.exe build.py
```

What this does:

- builds the Rust extension via `maturin`
- produces a wheel in `dist/`
- reinstalls the wheel into the current virtual environment in dev mode

This should be the standard deployment step for migration work.

## Why wheel reinstall matters

A common failure mode during this kind of mixed-language refactor is:

- source tree changed
- but the installed wheel is still old
- tests run against stale code

That can completely invalidate timing results and bug conclusions.

So for every phase that touches:

- `crates/`
- native bindings
- installed Python package contents

you must rebuild and reinstall before testing.

## Optional fast validation checks after build

After rebuilding, verify the active environment is using the expected installed code.

Examples:

```powershell
.\.venv\Scripts\python.exe -c "import running_process.pty as p; print(p.__file__)"
```

And when relevant:

```powershell
.\.venv\Scripts\python.exe -c "import inspect, running_process.pty as p; print('NativeProcess' in inspect.getsource(p))"
```

The point is to ensure:

- the correct module path is loaded
- the installed package contains the expected behavior

## Test-cycle deployment expectations

For a typical refactor phase, deployment should follow this order:

### Small Python-only phase

If only pure Python facade code changed:

1. `py_compile`
2. rebuild/reinstall anyway if the installed wheel includes those files
3. run targeted tests

### Native code phase

If Rust/native code changed:

1. `py_compile` for touched Python bindings/facades
2. `build.py`
3. verify the installed wheel
4. run targeted tests
5. run additional regression tests for impacted behavior

### Large integration phase

If `NativeProcess` behavior changes across both PTY and subprocess paths:

1. `py_compile`
2. `build.py`
3. targeted PTY tests
4. targeted subprocess tests
5. integration tests
6. optional performance trace run
7. commit if stable

## Suggested test-cycle command set

### Build/reinstall

```powershell
.\.venv\Scripts\python.exe build.py
```

### Targeted PTY regression

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_pty_support.py -ra
```

### Targeted single-test validation

Useful when chasing one behavior like EOF or wait semantics:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_pty_support.py -k expect_reports_pattern_not_found_on_eof -ra
```

### Additional subprocess regression

Run the relevant subprocess-focused tests after lifecycle changes.

The exact file list should be updated as the migration proceeds, but the principle is:

- PTY changes do not excuse skipping subprocess regression
- subprocess changes do not excuse skipping PTY regression

## Build/test success gate before commit

A phase is not ready to commit until:

1. build succeeds
2. targeted tests pass
3. the installed package is confirmed current
4. performance numbers are captured if the phase touched lifecycle or wait behavior

## Event Tracing for Performance Analysis

During the migration, event tracing should be the primary performance-analysis tool.

Thread dumps are useful for exploratory diagnosis, but event tracing is the better operational tool for:

- measuring timing
- attributing latency
- comparing phases
- proving that ownership moved in the intended direction

## Purpose of event tracing

The goal is to answer questions like:

- where is wall time actually going?
- are waits event-driven or still polling?
- how much time is spent between first output and exit?
- how much time is spent between exit observed and EOF?
- are reader threads still alive after exit?
- did a phase reduce CPU wakeups or just move them?

Tracing should make those answers explicit.

## Tracing design goals

The tracing system used during migration should be:

- opt-in
- low-overhead enough for targeted runs
- structured
- machine-readable
- phase-comparable

Recommended output format:

- JSONL

Recommended contents:

- monotonic timestamps
- event name
- pid
- tid
- backend kind
- phase/result
- duration
- object/process id
- timeout values
- counts for loops/wakeups if relevant

## What to trace

At minimum, trace native and facade events around:

- process start
- first output observed
- output chunk arrival
- exit observed
- EOF observed
- reader closed
- wait start/end
- expect start/end
- wait_for start/end
- idle wait start/end
- close/kill/terminate start/end

If native threads remain in play, also trace:

- reader thread start
- reader thread exit
- watcher thread start
- watcher thread exit

## Important latency intervals to measure

These intervals are especially valuable:

### Startup latency

- `start -> first_output`

### Execution latency

- `first_output -> exit_observed`

### Drain latency

- `exit_observed -> reader_closed`
- `exit_observed -> EOF_observed`

### Wait-path latency

- `wait_start -> wait_end`
- `expect_start -> expect_end`
- `wait_for_start -> wait_for_end`
- `wait_for_idle_start -> wait_for_idle_end`

### Teardown latency

- `close_start -> close_end`
- `kill_start -> kill_end`
- `terminate_start -> terminate_end`

## What tracing should prove during migration

Each migration phase should use traces to validate that ownership is actually moving downward.

Examples:

### After lifecycle migration

Tracing should show:

- fewer Python wait loops
- fewer polling wakeups
- native exit publication happening earlier and more consistently

### After PTY reader migration

Tracing should show:

- shorter `exit_observed -> reader_closed`
- fewer forced close fallbacks
- more direct EOF publication

### After output migration

Tracing should show:

- fewer repeated buffer rebuild steps in Python
- cleaner output progression

### After `wait_for` / `expect` migration

Tracing should show:

- fewer loop iterations
- fewer sleep intervals
- lower condition wait overhead

## Trace execution flow during a phase

For any phase that touches performance-sensitive behavior:

1. run targeted tests without tracing to get wall time
2. run targeted tests with tracing enabled
3. analyze the trace
4. compare against previous phase results
5. store key findings alongside the phase notes

That gives:

- real user-facing runtime
- diagnostic attribution

Both are necessary.

## Trace storage and comparison

For the migration, keep traces in a controlled location and name them by phase.

Example naming:

- `trace-phase-1.jsonl`
- `trace-phase-2.jsonl`
- `trace-phase-3-pty.jsonl`

This makes phase-to-phase comparison much easier than repeatedly overwriting one file.

## What not to do with tracing

1. Do not leave heavy tracing enabled permanently in the production path.
2. Do not treat one traced run as sufficient evidence.
3. Do not compare traced runtime directly to non-traced runtime without noting tracing overhead.
4. Do not rely only on wall-clock numbers when a trace can attribute the time precisely.

## Recommended tracing discipline per phase

For each performance-sensitive phase:

1. baseline non-traced run
2. traced run
3. summary of top latency contributors
4. one short written note:
   - what improved
   - what did not improve
   - what remains dominant

That note should be part of the phase record before the commit.
