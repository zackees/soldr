# Changelog

## 4.6.1 — user baseline env: restore USERNAME on Windows, real login env on Unix

Root-caused live on a dev box where soldr's daemon became permanently unreachable: `user_baseline_environment_block()` opened the process token with `TOKEN_QUERY` only, so `CreateEnvironmentBlock` **silently omitted the per-user dynamic variables** (`USERNAME`, `USERDOMAIN`). Consumers that key behavior on `USERNAME` (soldr derives its daemon pipe name from it) diverged from processes holding the real login environment — the daemon bound `soldr-daemon-soldr-<hash>` while every client dialed `soldr-daemon-<user>-<hash>`, forcing a permanent uncached-fallback loop.

- Windows: the token is now opened with `TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE` (the documented requirement for `CreateEnvironmentBlock`); regression test asserts `USERNAME` is present and matches the live login value.
- Unix: `EnvironmentPolicy::UserBaseline` no longer falls back to plain inheritance. It reconstructs a clean login environment from the user's identity via `getpwuid_r` — `USER`, `LOGNAME`, `HOME`, `SHELL`, the platform's default login `PATH` — carrying over `LANG`/`LC_*`/`TZ`/`TMPDIR` from the current process. Falls back to inheritance only when the passwd entry cannot be resolved. Process-local variables no longer leak into daemon baselines (regression test included).

## 4.5.11 — Windows: gate the CREATE_NO_WINDOW default on a console-less parent

Fixes [#622](https://github.com/zackees/running-process/issues/622): the #584/#585 `CREATE_NO_WINDOW` default was applied to **every** spawned child, not just daemon-spawned ones. A child forced onto its own invisible console can't receive `GenerateConsoleCtrlEvent` CTRL_C/CTRL_BREAK from a console-attached parent — which broke six KeyboardInterrupt integration tests on every Windows CI run since the 4.5.8 release, and breaks CTRL_C interop for any console-attached consumer.

- `windows_creation_flags` now takes `parent_has_console`; the `CREATE_NO_WINDOW` default applies only when the parent is console-LESS (the actual #584 flash scenario — a console-attached parent's child inherits the existing console, so no window can flash).
- Console attachment is probed with `GetConsoleCP() != 0`, **not** `GetConsoleWindow()`: hidden/windowless consoles (CI runners, agent harnesses) are attached consoles with a null window handle, and CTRL_C works across them.
- Explicit caller `creationflags` (`CREATE_NO_WINDOW` / `CREATE_NEW_CONSOLE` / `DETACHED_PROCESS`) still win in both directions.
- Validated RED→GREEN on Windows: the six broken tests (`test_live_pipe_interrupt_*`, `test_allows_child_ctrl_c_false_*`, `test_wait_raises_keyboard_interrupt_*`) fail on 4.5.10 and pass with this change in the same environment.

## 4.5.10 — Windows: private broker dirs no longer strip child / hardlinked-file ACLs

Fixes a destructive Windows DACL in `secure_dir` (the broker's private-dir hardening), root-caused live on a wedged dev box — see [zackees/soldr#1513](https://github.com/zackees/soldr/issues/1513).

The old owner-only DACL `D:P(A;;FA;;;OW)` carried a single **non-inheritable** ACE. Applying it with `SetNamedSecurityInfoW` re-propagates auto-inheritance to every existing descendant, which **stripped all children to an empty DACL** (deny-everyone, including the owner). Worse, NTFS hardlinks share one security descriptor per file, so a binary hardlinked inside a privatized dir bricked its sibling link *outside* the tree — e.g. a consumer's pip-installed `soldr.exe` became unreadable/unexecutable until manually repaired with `icacls /reset`.

- The private-dir DACL is now `D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)`: still protected + private from other users, but the ACEs are inheritable, so propagation *grants* owner + SYSTEM access down the tree instead of stripping it. SYSTEM is included so AV / indexing / backup agents keep working.
- `private_dir_permissions_are_private` now rejects the legacy non-inheritable shape, so probe-and-repair callers re-apply the fixed DACL — which also **self-heals** trees bricked by the old one.
- Windows regression tests cover the child-strip, the hardlink leak, and the legacy-shape heal path.

Unix (`chmod 700`) is unchanged.

## 4.4.0 — `into_backend_io()`: hand the live broker socket back to the consumer

Adds [`BrokerSession::into_backend_io`](https://github.com/zackees/zccache/issues/720) (and its `AsyncBrokerSession` twin) so a consumer that has finished broker adoption can stop speaking the FrameV1 request/response wire and take ownership of the live negotiated socket to run its own protocol over it.

- New `BrokerSession::into_backend_io() -> Result<OwnedBackendIo, IntoBackendIoError>` and `AsyncBrokerSession::into_backend_io()`. On Unix `OwnedBackendIo::into_owned_fd()` yields an `OwnedFd` (and `OwnedBackendIo: AsFd`) that wraps directly into a `std::os::unix::net::UnixStream`.
- Windows `OwnedHandle` support is deferred; `into_backend_io()` returns `IntoBackendIoError::WindowsUnsupported` there for now.
- Supporting surface: `FrameClient::buffered_len()` / `into_stream()` and `AsyncFrameClient::into_blocking()`.
- The frozen FrameV1 wire (`0x7A63` payload protocol) is untouched — this is purely an additive escape hatch from the frame lane to the raw socket.

Additive only; no existing API changes and the Python ABI is unchanged.

## 4.0.1 — Restore public access to PTY backend traits

Surfaced by clud during the 4.0.0 rollout (see [meta tracker](https://github.com/zackees/running-process/issues/203)).

In 4.0.0 the new `pty::backend` module was `pub(crate)` along with its `PtyMaster` / `PtyChild` / `PtySize` types. That left downstream consumers in an awkward state: `NativePtyHandles.master` is `pub` and typed as `Box<dyn PtyMaster>`, but the trait wasn't reachable, so callers could hold the box but couldn't call `resize()` on it — a regression from the 3.x portable-pty surface.

This patch:

- Promotes `pty::backend` to `pub`, and `PtyMaster` / `PtyChild` / `PtyBackend` / `PtySlave` / `PtySize` to `pub`.
- Re-exports `PtyMaster`, `PtyChild`, `PtySize` at `running_process::pty::*` for convenience.
- Adds `PtyMaster::get_size()` — Windows caches the last-set size internally (ConPTY has no live query API); Unix delegates to portable-pty.
- New integration test `pty_master_public_api_test.rs` locks the surface.

No other API changes. Python ABI unchanged.

## 4.0.0 — Mono-crate consolidation

**Breaking change for direct Rust consumers only.** The Python (`pip install running-process`) ABI is unchanged.

The `running-process-core`, `-proto`, `-client`, `-daemon`, and `daemon-trampoline` crates have been merged into a single **`running-process`** crate with feature-gated subsystems. Only `running-process-py` remains separate as the PyO3 binding (per the [language-bindings-only resolution in #165](https://github.com/zackees/running-process/issues/165)).

### Migration for direct Rust consumers

| Before | After |
|---|---|
| `running-process-core = "3.x"` | `running-process = "4.0.0"` (with `default-features = false` if you want only the spawn API) |
| `running-process-proto = "3.x"` | `running-process = "4.0.0", features = ["client"]` (proto types reachable at `running_process::proto::*`) |
| `running-process-client = "3.x"` | `running-process = "4.0.0", features = ["client"]` (default; reachable at `running_process::client::*`) |
| `running-process-daemon = "3.x"` | `running-process = "4.0.0", features = ["daemon"]` (reachable at `running_process::daemon::*`) |
| `cargo install runpm-cli` | `cargo install running-process` (the default `client` feature pulls `runpm` in) |
| `cargo install running-process-daemon` | `cargo install running-process --features daemon --bin running-process-daemon` |

### Feature scheme

- **`core`** (always available) — spawn API, PTY, containment.
- **`client`** (default) — proto types + sync IPC client to talk to a daemon. Adds prost, interprocess, dirs.
- **`daemon`** — full daemon runtime. Superset of `client`. Adds tokio, rusqlite, tracing, etc.
- **`originator-scan`** — used by `running-process-py` for cwd-tagging.

### Why

- **Publish surface:** 6 crates → 2 (running-process + running-process-py). Single version-bump motion at release time.
- **Dependency clarity:** consumers pick the subsystem they need with one feature flag instead of three path-deps. Tree-shaking via `required-features` on binaries means `cargo install` builds only what the chosen binary needs.
- **No more cross-crate plumbing:** ~120 `running_process_core::` / `running_process_proto::` / `running_process_client::` / `running_process_daemon::` imports collapsed to `crate::*` (lib) or `running_process::*` (binaries / tests).

### Python — unchanged

The PyPI `running-process` wheel ships with the same Python API. Upgrade as normal.

### Internal-only crates (unchanged)

- `crates/test-watchdog/` — Windows hang-dump helper (publish=false, dev-dep).
- `testbins/` — 8 test-fixture binaries (publish=false).

### Forward-looking

A future Go binding will live in its own repo (`zackees/running-process-go`) per the [Q3 resolution in #165](https://github.com/zackees/running-process/issues/165). Same pattern can support `running-process-node` or other language bindings; each gets its own crate alongside `running-process-py`.

---

Older releases are recorded in the GitHub Releases page.
