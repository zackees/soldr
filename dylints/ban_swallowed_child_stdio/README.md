# `ban_swallowed_child_stdio`

Rejects production code that discards a child's stdout or stderr to a null
sink (soldr#3387, meta soldr#3389): `Command::stdout(Stdio::null())` /
`Command::stderr(Stdio::null())` for std and Tokio commands, and
`running_process::SpawnStdio` / `DaemonStdio` with `stdout` or `stderr` set to
`Null`. A null `stdin` is always allowed.

Small tools route through `soldr_core::core::tool_output`, whose contract is
**always forward + always log**: stderr is forwarded to Soldr's stderr with a
tool prefix, every run is recorded in `<soldr root>/logs/small-tools.jsonl`,
and a failure's error carries a stderr excerpt.

A site that must discard output opts out with the reason beside it:

```rust
// reason: <why discarding this output is correct>
#[cfg_attr(dylint_lib = "ban_swallowed_child_stdio", allow(ban_swallowed_child_stdio))]
```

Test crates (`#[cfg(test)]` code and `tests/` targets) and examples are out of
scope. The lint cannot see output that is captured and then dropped, a
`Stdio::null()` bound to a local first, or Python and shell code.
