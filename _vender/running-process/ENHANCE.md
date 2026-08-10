# Enhancement: First-Class stdin/filter API

## Problem

`RunningProcess.run(...)` is a strong default for captured subprocess execution because it continuously drains output and avoids pipe-buffer deadlocks.

There is still a missing use case: commands that are primarily **stdin-driven filters** rather than plain command execution.

Examples:

- clipboard tools like `clip.exe`, `pbcopy`, `xclip`
- filter-style CLIs like `diff2html`
- commands like `crontab -` that expect the full payload on stdin

Today these are still better served by:

```python
proc = subprocess.Popen(..., stdin=PIPE, stdout=PIPE, stderr=PIPE)
stdout, stderr = proc.communicate(input=payload, timeout=...)
```

That is safe when used correctly, but it means callers have to drop below the `running-process` abstraction for an important subprocess pattern.

## Requested API

Add first-class support for request/response subprocesses that need explicit stdin input.

Possible shapes:

```python
RunningProcess.run(
    args,
    input=b"...",
    capture_output=True,
    timeout=5,
)
```

or

```python
RunningProcess.run_filter(
    args,
    input=b"...",
    timeout=5,
)
```

## Requirements

- support `input=` in bytes and text forms
- preserve the package's anti-deadlock guarantees while stdin is being sent
- return a `CompletedProcess`-like result
- define stderr behavior clearly
  - either preserve separate stderr
  - or document merged stderr explicitly
- support timeout during both stdin write and output drain
- keep current `RunningProcess.run(...)` behavior unchanged for existing callers

## Why this matters

Without this, downstream code still needs direct `Popen(...).communicate(...)` for a legitimate and common subprocess pattern. That weakens the goal of having one safe, high-level subprocess API.

The use case came up while migrating a codebase away from captured `subprocess.run(...)` calls. We were able to move normal capture cases to `RunningProcess.run(...)`, but stdin-filter calls still required manual `Popen(...).communicate(...)`.

## Integration Update: `clud` hook runner

Date: 2026-04-08

While integrating `running-process` into `clud`, two additional gaps showed up that are worth tracking here even though the current repo has already moved beyond the older released API.

### 1. Released `RunningProcess.run(...)` API was too narrow for real `subprocess.run(...)` replacement

In the installed package used by `clud`, `RunningProcess.run(...)` only accepted a small positional surface:

- `command`
- `cwd`
- `check`
- `timeout`
- `on_timeout`

That meant downstream code could not use the high-level helper for legitimate cases like:

- `shell=True`
- `env=...`
- `stdin=subprocess.DEVNULL`
- `capture_output=True`
- `text=True`
- `encoding=...`
- `errors=...`

Result: callers had to instantiate `RunningProcess(...)` directly just to get safe captured execution with stdin isolation.

What to preserve going forward:

- keep `RunningProcess.run(...)` as the primary subprocess-style entrypoint
- ensure its signature continues to accept common `subprocess.run(...)`-style kwargs
- test the public `run(...)` API, not just the lower-level constructor path

### 2. Explicit stdin-isolation support matters for hook subprocesses

`clud` runs shell hooks that must not inherit or consume the parent terminal's interactive stdin. The practical requirement is:

```python
RunningProcess.run(
    command,
    shell=True,
    cwd=...,
    env=...,
    stdin=subprocess.DEVNULL,
    capture_output=True,
    text=True,
    timeout=...,
)
```

This is not just a stdin-filter/input case. It is a "captured command with detached stdin" case. That pattern should stay supported and documented because it is common for:

- pre/post-edit hooks
- git hooks
- background helper commands
- validation commands that should never block on terminal input

### 3. Versioned docs should call out stdout/stderr behavior changes clearly

The older package merged stderr into stdout in its `run(...)`/`subprocess_run(...)` helpers. The current Rust-backed repo now returns separate `stdout` and `stderr` and exposes `combined_output` separately.

That is a reasonable improvement, but downstream adopters need that called out prominently in release notes because wrappers and tests often encode assumptions about:

- whether `stderr` is always `None`
- whether nonzero command output is found in `stdout`
- whether the "combined" stream is the default compatibility view

## Suggested follow-up

- add regression tests for `RunningProcess.run(...)` covering `shell`, `env`, `stdin=DEVNULL`, `capture_output`, and `text`
- document the intended compatibility target versus `subprocess.run(...)`
- document the stdout/stderr split as a breaking or behaviorally significant change when publishing the next release
