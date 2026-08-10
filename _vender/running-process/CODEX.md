# Codex Agent Documentation

First rule: direct Cargo build/check/test/package/publish commands in this repo must be prefixed with `soldr` (the globally installed binary), or you should use the higher-level repo entrypoints that already choose the compatible path for you (`uv run build.py`, `./install`, `./lint`, `./test`). Do not run raw `cargo build`, `cargo check`, `cargo test`, `cargo package`, `rustc`, or `rustfmt` directly — the `PreToolUse` hook denies them.

Read [CLAUDE.md](C:\Users\niteris\dev\running-process\CLAUDE.md) for the rest of the agent documentation and repository guidance.

Codex project config lives in [.codex/config.toml](C:\Users\niteris\dev\running-process\.codex\config.toml) and [.codex/hooks.json](C:\Users\niteris\dev\running-process\.codex\hooks.json). The checked-in Codex `PreToolUse` hook denies raw build-tool Bash commands unless they already go through `soldr` or the higher-level repo entrypoints.

Codex execpolicy rules also live in [.codex/rules/soldr.rules](C:\Users\niteris\dev\running-process\.codex\rules\soldr.rules) and forbid raw soldr-supported direct build commands.
