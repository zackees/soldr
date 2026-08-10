# running-process — crate source

Top-level modules of the published `running-process` crate. Feature gating is
described in the repo-root `CLAUDE.md` ("Rust workspace" section).

- `lib.rs` / `public_symbols.rs` — crate surface and re-exports
- `environment.rs` — user baseline environment (Windows `CreateEnvironmentBlock`, Unix login reconstruction)
- `spawn.rs`, `spawn_imp_unix.rs`, `spawn_imp_windows.rs` — process spawn implementations
- `process_tree.rs` — process-tree enumeration and tree kill
- `containment.rs` — Job Objects / process-group containment
- `console_detect.rs`, `terminal_graphics.rs`, `pty/` — console + PTY support
- `client/`, `daemon/`, `broker/`, `observer/` — IPC client, daemon runtime, broker tiers, observer sidecar
- `bin/` — `runpm`, `daemon`, `trampoline`, broker + cleanup binaries
- `boot_autostart/`, `maintenance/`, `cleanup/` — lifecycle helpers
- `originator.rs`, `runpm_config.rs`, `systemd_killmode.rs`, `unix.rs`, `windows.rs`, `helpers.rs`, `types.rs` — platform + config plumbing
- `test_support/`, `tests.rs`, `rust_debug.rs` — test-only support
