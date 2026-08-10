# Rust PTY Guidance

`running-process` exposes two different Rust transports:

- `NativeProcess` for normal subprocess semantics.
- `NativePtyProcess` for terminal semantics.

Choose deliberately.

## When To Use PTY

Use `NativePtyProcess` or `InteractivePtySession` when the child expects a real
terminal:

- TUIs and REPLs
- shells
- programs that redraw lines with `\r`
- programs that emit terminal queries such as `\x1b[6n`

`InteractivePtySession` is the safe default for Rust consumers because it can
own the pieces that a terminal-style session usually needs:

- output echo
- terminal input relay
- PTY query replies
- interrupt routing
- resize and teardown through the underlying `NativePtyProcess`

## When PTY Is The Wrong Transport

Do not use PTY as the default for:

- one-shot prompt execution
- batch jobs
- worker subprocesses
- backend calls that should not inherit arbitrary parent terminal input

PTY is not just "subprocess with nicer output". It changes semantics:

- stdout/stderr become terminal-style output instead of normal pipes
- terminal control bytes are preserved
- callers must think about input relay, resize, interrupt, and cleanup

For noninteractive work, use `NativeProcess`.

## Recommended Rust Entry Point

```rust
use running_process::pty::{InteractivePtySession, NativePtyProcess};

let process = NativePtyProcess::new(
    vec!["python".into(), "-i".into()],
    None,
    None,
    24,
    80,
    None,
)?;
let session = InteractivePtySession::new(process);
session.start()?;

loop {
    let pumped = session.pump_output(Some(0.1), true)?;
    if pumped.stream_closed {
        break;
    }
}

let code = session.wait(Some(30.0))?;
```

On Windows ConPTY, final teardown can still require `close()` or `kill()` to
drop PTY handles after the child exits.

If you need the raw primitive, keep using `NativePtyProcess`, but treat
`InteractivePtySession` as the canonical recipe for a full interactive
terminal session.
