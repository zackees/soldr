# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Architecture Overview

A Rust-backed Python library (v4.6.4) for subprocess and PTY process management across Windows, macOS, and Linux.

### Layered Design

**Python layer** (`src/running_process/`) provides high-level APIs:
- **`RunningProcess`**: Pipe-backed subprocess wrapper with output streaming, process tree management, and timeout handling
- **`PseudoTerminalProcess`**: PTY-backed process wrapper with expect patterns, idle detection, and terminal input relay
- **`InteractiveProcess`**: Unified facade dispatching to either pipe or PTY backends
- **`ProcessOutputReader`**: Threaded reader draining stdout/stderr to prevent blocking
- **`RunningProcessManager`**: Thread-safe singleton registry for tracking active processes
- **`probe`** (`probe.py`): enrolls a Python process with the probe daemon (`install()` → `ProbeGuard`, reported as `runtime=python`) and captures **mixed-mode** stacks — `snapshot()` pairs native frames from the Rust capture with `sys._current_frames()` interpreter frames, aligned by OS thread id. `write_dump()` writes both halves as one artifact. Enrollment never blocks; a build without the `probe` feature degrades to a no-op.
- **`dump_paths`**: the diagnostic-artifact location and naming convention, shared by the CLI supervisor and the probe so evidence lands in one place

**Rust workspace** (`crates/`). Four crates publish — `running-process`,
`running-process-probe`, `running-process-probe-daemon`, and
`running-process-py` — and a release bumps all of them in lockstep
(`ci/version_check.py` enforces it). The rest are `publish = false` and exist
only inside this repo:
- **`running-process`** (`crates/running-process/`): the crate consumers depend on. Feature-gated subsystems:
  - **`core`** (always on) — OS-level subprocess abstraction (`NativeProcess` — pipe I/O, signaling, Job Objects/process groups).
  - **`pty`** — PTY-backed process APIs (`portable-pty` on Unix, ConPTY on Windows).
  - **`client`** (default) — proto types (`src/proto/`) + sync IPC client (`src/client/`). Adds prost, interprocess, dirs.
  - **`daemon`** — full daemon runtime (`src/daemon/`). Adds tokio, rusqlite, tracing, etc.
  - **`probe`** — the probe client facade (`src/probe/`): `probe::install(Config)` synchronously prepares the owner-private crash spool/native handler, then enrolls this process with `rpprobed` on a background thread and returns a `Guard` that deregisters on drop. Daemon I/O remains asynchronous, so an absent daemon never slows startup. Pulls in `running-process-probe` for the proto types.
  - Binaries in `src/bin/`: `runpm` (requires `client`), `daemon` (requires `daemon`), `trampoline` (no required-features), `running-process-broker-v1` / `running-process-broker-v2` (broker scaffold for #483/#488/#532), `running-process-cleanup`.
  - `proto/daemon.proto` compiled by `build.rs` (prost-build + protox).
- **`running-process-py`**: PyO3 bindings. Contains `NativePtyProcess` alongside the pipe backend. Exposes a unified `PyNativeProcess` with `NativeProcessBackend` enum dispatching to either `NativeRunningProcess` or `NativePtyProcess`. Depends on `running-process` with `features = ["client", "originator-scan", "probe"]`, and on `running-process-probe` for the snapshot API. Exposes `native_probe_install` / `native_probe_uninstall` / `native_probe_is_armed` / `native_probe_snapshot`.
- **`running-process-probe`** (`crates/running-process-probe/`, published): two things now. (a) **`snapshot`** (#635) — cooperative all-thread stack capture on Windows, Linux, and macOS (x86_64 + aarch64): suspend or signal siblings, copy registers + a bounded readable stack slice, resume, then unwind PE/ELF/Mach-O metadata with `framehop` *after* every thread is running again. Linux's handler touches only atomics and macOS copies through Mach VM reads, so invalidated mappings degrade to dropped samples rather than host faults. Linux reserves one otherwise-unused realtime signal on first capture; replacing that disposition or consuming it through `signalfd` afterward violates the process-lifetime ownership contract. Also `modules`, `unwind`, and `stream` (bounded drop-and-count sink). (b) **`probe_diag::v1`** — the prost types for the probe protocol, compiled from `proto/probe_diag_v1/`. (c) the original sidecar / file-hook tier for #539 follow-up #551. Behind the off-by-default `embed-helper` feature flag (`dep:dirs`, `dep:blake3`, and on Windows `dep:windows-sys`). Exposes `HookConfig`, `negotiate_hook_support()`, embed-and-extract cache (`helper_cache_dir`, `extract_helper_blob_to`), and the per-OS injection vehicles `inject_into_pid` (Windows) / `inject_via_env` (Linux + macOS). **Sidecar contract**: this is the ONLY place injection symbols may live — main `running-process` crate stays free of `CreateRemoteThread` / `dlopen` of interposers (enforced for AV / EDR static analysis).
- **`running-process-probe-daemon`** (`crates/running-process-probe-daemon/`, published): the `rpprobed` daemon. Owns the control socket (single-instance via bind), the in-memory registration `registry`, the sans-io `probe_ops` dispatcher, `serve` (request loop; connection close is the liveness signal), `discovery` (owner-only discovery file), and `symbolication` (spawns and supervises the worker). Peer credentials are read off the socket and the registry owner is derived from the same source as the peer policy.
- **`running-process-probe-worker`** (`crates/running-process-probe-worker/`, publish=false): the off-process symbolizer — a stdin→stdout filter taking one capture per invocation. `wire` carries ASLR-independent `(module_index, relative_address)` frames; `discovery` performs deterministic manifest/path/cache/server lookup with exact ELF build-id, Mach-O UUID, or PDB GUID+age gates; and `symbolize` resolves PDB/ELF/Mach-O function names. **Isolation is a process boundary, not a `catch_unwind`**: a malformed symbol file can crash a parser outright rather than returning an error, so the daemon spawns this as a short-lived child and reads its exit status. This crate is where symbol-file parsers belong — they must never enter `running-process` or `running-process-py`, so the long-lived daemon does not even link them.
- **`running-process-probe-interposer-{linux,macos,windows}`** (publish=false): per-OS cdylib + rlib interposers that ship the actual file-API detours (`open`/`openat`/`close`/`write`/`unlink`/`rename` and Windows equivalents — `CreateFileW`/`WriteFile`/`CloseHandle`/`DeleteFileW`/`MoveFileExW`). Linux uses `LD_PRELOAD` + `dlsym(RTLD_NEXT, …)`; macOS uses `DYLD_INSERT_LIBRARIES` (SIP / hardened-runtime carve-outs apply); Windows uses `retour::RawDetour` inline trampolines, gated on `x86_64` only (`retour 0.4.0-alpha.4` uses iced-x86 which doesn't support ARM64). Each emits `RPP_HOOK …` lines on stderr in a shared format. Non-target hosts compile to an inert rlib stub so the workspace builds end-to-end.
- **`running-process-win-gnu-bridge`** (`crates/running-process-win-gnu-bridge/`, publish=false): build seam (#580) exposing the MSVC-obligatory Windows API surface to `x86_64-pc-windows-gnu` builds. Inert no-op on MSVC / non-Windows; on `-gnu` it statically imports the ConPTY entry points (`CreatePseudoConsole` / `ResizePseudoConsole` / `ClosePseudoConsole`) directly from `windows-sys` (which bundles a per-target `-gnu` import lib), proving the surface links with no Windows SDK / MSVC `link.exe`. `retour` detours / DLL injection and the bundled `libsqlite3-sys` daemon build are validated under GNU; the daemon path needs MinGW-w64 `gcc.exe` on `PATH`. See `docs/win-gnu-bridge.md`.
- **`test-watchdog`** (`crates/test-watchdog/`, publish=false): cross-platform hang-dump helper used as dev-dep by `running-process` tests (procdump minidump on Windows, gdb/lldb all-thread backtraces on Unix).
- **`testbins`** (`testbins/` at the repo root, not under `crates/`): test-fixture binaries (`cwd-reporter`, `dies-after-spawn`, `emitter`, `env-dump`, `env-reporter`, `sleeper`, `slow-stdin-reader`, `spawner`, `stdin-echoer`, `stubborn`, `tui-counter`, `createfilew-probe`).

**Python-Rust bridge**: `running_process._native` module compiled via maturin. Python's `PseudoTerminalProcess.start()` calls `NativeProcess.for_pty()` which creates a `NativePtyProcess` on the Rust side.

## Development Commands

**Build (native extension):**
```bash
uv run build.py              # Dev wheel, reinstalls into venv (default)
uv run build.py --release    # Publish-grade wheels in dist/
```

**Testing:**
```bash
./test                                                  # Full suite: Rust tests + dev build + pytest
soldr cargo build -p testbins                           # REQUIRED before a bare `nextest` run (see below)
uv run --no-sync pytest tests -v                        # Python tests only (preserves the existing venv)
uv run --no-sync pytest tests/test_foo.py -v            # Single test file
uv run --no-sync pytest tests/test_foo.py::TestClass::test_method -v  # Single test
RUNNING_PROCESS_LIVE_TESTS=1 uv run --no-sync pytest -m live tests -v  # Integration tests
```

**Test fixtures are built once, up front.** The Rust tests locate the
`testbins` binaries by path; they no longer build them on demand. `./test` and
CI do this for you, but a bare `soldr cargo nextest run -p running-process`
does not, and the tests will fail naming the missing fixture and the command
to run.

They used to build themselves, once per call. That took cargo's
build-directory lock, and nextest gives each test its own process, so a
full-suite run had dozens of cargo invocations queued on one lock — which
presented as an unexplained 30s+ hang (#747). Any new way of invoking the
suite has to build the fixtures first, with a matching `--target` /
`--target-dir` when the tests do not run against the host tree.

**`uv run` policy.** Bare `uv run …` is **blocked by the pre-tool hook** because it auto-syncs the maturin project and forces a full native rebuild on every invocation (see zackees/soldr#805). Always pass `--no-project` for pure-Python scripts, `--no-sync` to reuse the warm venv, or `--frozen` to lock to the existing lockfile. The escape hatch for a legitimate full-rebuild is `./test`.

**Per-test deadlock guard.** Every test (Rust + Python) gets a hard 2-minute wall-clock kill so a hung test can't stall CI indefinitely:
- Rust runs through `cargo nextest` (auto-installed by `ci/test.py` if missing); `.config/nextest.toml` sets `slow-timeout.terminate-after = 2 × 60s`. On fire nextest prints `TIMEOUT [...] <crate>::<test_file> <test_name>` plus captured stdout/stderr.
- Python uses `pytest-timeout` with `timeout = 120, timeout_method = "thread"` in `pyproject.toml`. On fire pytest prints a `+++ Timeout +++` banner with every thread's Python stack — enough to identify the hung test from CI logs.
- Rust tests that opt into `test_watchdog::install(timeout, message, dump_path)` (e.g. `tests/containment_test.rs`) additionally get an out-of-process dump *before* nextest's kill: on Windows a full minidump via `procdump -ma`; on Linux/macOS all-thread backtraces via `gdb -p <pid> -batch -ex 'thread apply all bt'` (or `lldb --batch -o 'thread backtrace all'`), printed to stderr and written to `dump_path`. Works for non-cooperative hangs (thread blocked in a syscall); on Linux the watchdog sets `PR_SET_PTRACER_ANY` so the child debugger may attach even under Yama `ptrace_scope=1`. Missing debugger → one-line note, never an extra failure.
Override per-invocation when needed: `cargo nextest run -- --slow-timeout 30s --terminate-after 1` or `pytest --timeout=300`.

**Individual CI stages** (same modules the workflows invoke, so a failing job
can be reproduced locally instead of reassembled from workflow YAML):
```bash
uv run --no-sync python -m ci --help          # list the stages
uv run --no-sync python -m ci guard-jemalloc  # run one guard
uv run --no-sync python -m ci lint            # what `./lint` wraps
```

**Linting:**
```bash
./lint                           # Full suite: ruff + black + isort + pyright + KBI checker + spawn-path-guard
uv run ruff check --fix src tests
uv run black src tests
uv run pyright src tests
```

The lint pass also runs `ci/spawn_path_guard.py`, which forbids raw `Command::new` / `.spawn()` / `portable_pty` / `CreatePipe` / `ChildStd*::from` outside the sanitized spawn layer. New call sites need an explicit allowlist entry with a justification comment — see existing entries for the shape.

**Wrong toolchain?** Invoke build commands as `soldr cargo …`, `soldr rustc …`, `soldr rustfmt …`. The globally installed [soldr](https://github.com/zackees/soldr) binary resolves the rustup-managed toolchain via `rustup which` — handy on Windows where chocolatey cargo or other stale shims can take precedence on PATH. Install soldr globally (it is no longer pulled in as a uv dev dep) — e.g. `pipx install soldr` or `cargo install soldr`. CI Python (`ci/soldr.py:cargo_command`) detects soldr on PATH and routes through it automatically, falling back to raw `cargo` on CI runners where soldr isn't installed.

**Cross-compiling? Use `soldr build --target <triple>`, not `soldr cargo build --target`.**
`soldr build` is soldr's blessed cross-compile surface: it prepares the target
sysroot and the compiler/linker environment, including the managed xwin cache
with clang/lld for `*-pc-windows-msvc`. This is how `auto-release.yml` builds all
six binary targets from one Linux runner family, and `ci/cross_compiler_guard.py`
fails lint if anyone reintroduces `cargo-zigbuild`, `cargo-xwin`, `cross`, the
`ziglang` package, maturin's zig flag, or the `maturin[zig]` extra.

**Known hazard — `soldr build` cannot build `testbins` right now.** soldr ships a
prebuilt *upstream* mimalloc in its syslib catalogue and injects it for any crate
declaring `links = "mimalloc"`. `mimalloc-pprof` is a fork with extra symbols
(`mi_prof_*`, `mi_unwrapped_*`), so the substitution drops them and the link dies
with `LNK2019: unresolved external symbol mi_prof_start_ex`:

```
soldr build -p testbins --target x86_64-pc-windows-msvc   # fails
soldr cargo build -p testbins --target x86_64-pc-windows-msvc   # works
```

Use `soldr cargo build` for anything pulling in `mimalloc-pprof` until
[zackees/soldr#2142](https://github.com/zackees/soldr/issues/2142) is fixed. The
release workflow is unaffected because it builds only `runpm` and
`running-process-daemon`, neither of which links that crate — but that stops being
true the moment heap profiling ships in a released binary.

Only forks are affected: `zstd-sys` (`links = "zstd"`) and `libsqlite3-sys`
(`links = "sqlite3"`) also hit the catalogue and substitute cleanly, because they
are stock upstream.

**Environment:**
```bash
. ./activate.sh              # Activate dev environment (git-bash on Windows)
./install                    # Bootstrap Rust toolchain; builders use soldr
```

Project hook policy: `.claude/settings.json` mandates that direct soldr-supported Bash build commands (`cargo build|check|test|package|publish`, `rustc`, `rustfmt`, `clippy-driver`) are prefixed with `soldr` (the globally installed binary). Raw commands are denied — use `soldr cargo ...` or one of the higher-level repo entrypoints (`uv run build.py`, `./install`, `./lint`, `./test`).

## Daemon

```bash
running-process-daemon start|stop|status|list|kill-zombies
```

**Environment variables:**
- `RUNNING_PROCESS_NO_TRACKING=1` — disable daemon IPC
- `RUNNING_PROCESS_DAEMON_SCOPE=dev` — CWD-scoped daemon for test isolation
- `RUST_LOG=debug` — daemon log level
- `RUNNING_PROCESS_FAKE_BACKEND=<path>` — TEST-ONLY broker seam: `connect_to_backend` dials `<path>` directly, skipping broker negotiation entirely (never set in production; `RUNNING_PROCESS_DISABLE=1` takes precedence)
- `RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED=1` — opt out of the broker-v2 "refuse privileged startup" guard (test-only; defaults to refusing root)
- `RUNNING_PROCESS_BROKER_OWNED_BIND=0` — fall back to spawn-then-probe. **On
  by default** (#500 slice 32): the broker binds the backend endpoint itself
  and hands the listener to the daemon, so the endpoint is listening — and
  clients queue in the accept backlog — before the daemon's `main` runs.
  Unix-only; `broker_owned_bind::support()` reports the Windows gap with a
  reason and the spawn-then-probe path applies there regardless of this
  variable. Socket cleanup: a failed launch is cleaned up by the broker
  (#826), a broker-initiated teardown after the exit is confirmed (#828), and
  a daemon that exits on its own leaves its endpoint behind — accepted rather
  than swept, because the allocator issues a fresh path per launch so a stale
  entry is never reused, and a sweep would have to decide a socket is dead
  while a daemon might still hold it.
- `RUNNING_PROCESS_BROKER_LISTENER_FD=<fd>` — set by the broker, read by the
  daemon. Not for hand-setting: it names the descriptor the broker passed, and
  `bootstrap` adopts it instead of binding. A value naming anything but a
  listening socket is refused rather than adopted, so a stray setting fails the
  daemon's start rather than silently aliasing a descriptor it already owns.
- `RUNNING_PROCESS_PROBE_LINE_NUMBERS=1` — resolve `file:line` for probe stack
  frames as well as function names (#803). Off by default because line numbers
  parse a line program per module, and a caller who only wants "which function"
  should not pay for it. Works on all three platforms and both discovery tiers;
  on Windows a *local* build needs its PDB, which soldr's cached build currently
  drops (zackees/soldr#2148).

## Broker

`running-process-broker-v2` is the v2 transport (see #483 / #488 / #532). Bind path derivation lives in `src/broker/lifecycle/names_v2.rs` (`rpb-v2-{program}-{sid_hash}-{pipe_idx}`); the resolved socket path goes under `$XDG_RUNTIME_DIR/running-process/broker-v2/` on Linux, `$TMPDIR/.rp-<uid>-broker-v2/` on macOS (hashed leaf to fit `sun_path`), or `\\.\pipe\…` on Windows. `is_already_bound_error` classifies `AddrInUse | WouldBlock | PermissionDenied` as already-bound — `PermissionDenied` is included because Windows double-bind surfaces as `ERROR_ACCESS_DENIED` (raw os error 5) via the existing pipe instance's ACL.

## File-Hook Tier (#551)

Off-by-default opt-in via the `running-process-probe` crate's `embed-helper` feature. When enabled, `negotiate_hook_support()` returns `HookSupport::Available` on Windows + Linux + macOS. The injection vehicle is per-OS:

- **Windows**: `inject_into_pid(pid, dll_path)` drives `OpenProcess` → `VirtualAllocEx` → `WriteProcessMemory` → `CreateRemoteThread(LoadLibraryW, dll_path)` → `WaitForSingleObject` + `GetExitCodeThread`. The injected DLL's `DllMain` defers `retour::RawDetour` install to a `CreateThread` worker (retour's iced-x86 prologue analysis + `VirtualProtect` re-enter the loader lock; inline install hangs `LoadLibraryW`).
- **Linux + macOS**: `inject_via_env(command, interposer_path)` sets the platform's loader env var (`LD_PRELOAD` / `DYLD_INSERT_LIBRARIES`) on a caller-supplied `Command`. The dynamic linker handles the rest at child startup.

The interposers emit `RPP_HOOK …` lines on the target's stderr (e.g. `RPP_HOOK file-open path="…" access=0x… disposition=… handle=…`). All injection symbols live in the probe crate; the main `running-process` crate compiles with **zero** new injection-related symbols (verified end-to-end).

## CLIs

Two entry points in `pyproject.toml`:
- `running-process` → `running_process.cli:main` (daemon control, process listing)
- `running-processor` → `running_process.processor_cli:main` (dashboard web UI)

## Releasing

Releases are driven by the **Auto Release** workflow (`.github/workflows/auto-release.yml`).

Full operator guide — trigger conditions, one-time prerequisites
(PyPI trusted publisher, `CARGO_REGISTRY_TOKEN`), the version-bump
checklist that `ci/version_check.py` enforces, what each job
publishes, and recovery for common failure modes — lives in
[docs/RELEASING.md](docs/RELEASING.md).

Quick local sanity check before cutting a release:
```
uv run --no-project --module ci.version_check
```
(`--no-project` skips the maturin auto-sync — `ci.version_check` only reads version strings out of `pyproject.toml`/`Cargo.toml`/`__init__.py` and doesn't need the native module.)

## Agent Backlog

Active pending work lives in [docs/AGENT_TASKS.md](docs/AGENT_TASKS.md). Root-level scratch task files are historical breadcrumbs.

## Windows Native Build Rules

- The canonical local rebuild path is `uv run build.py` — do not use raw `cargo build`
- `uv run build.py --dev` and `uv run build.py --quick` are the same mode
- Prefer repo entrypoints (`./install`, `./test`, `./lint`, `uv run build.py`) over ad hoc cargo commands
- When a native dependency needs a C compiler, run from a Visual Studio developer shell or through `VsDevCmd.bat`
- Force the build target to `x86_64-pc-windows-msvc` when the environment is ambiguous. When intentionally building `x86_64-pc-windows-gnu` with the `daemon` feature, ensure MinGW-w64 `gcc.exe` is on `PATH` so bundled `libsqlite3-sys` can compile sqlite.
- If a rebuild behaves like a GNU build on Windows, check the active shell environment before changing Rust code

## Code Conventions

**Imports**: Use fully qualified absolute imports (`from running_process.module import Class`, not relative `from .module import Class`)

**Subprocess commands**: Use `subprocess.list2cmdline()` instead of `str.join()` for proper shell escaping

**Output buffering**: `PYTHONUNBUFFERED=1` is automatically set for all spawned processes in `_create_process_with_pipe()` and `_create_process_with_pty()`

**Testing**: Use `unittest` framework (TestCase, assertEqual, etc.). Pytest is only the runner — avoid pytest-specific fixtures and decorators.

**Keyboard interrupts**: Use `handle_keyboard_interrupt(exception)` from `running_process.interrupt_handler` instead of directly calling `_thread.interrupt_main()`. The KBI linter (`ci/lint_python/keyboard_interrupt_checker.py`) enforces this as part of `./lint`, scoped to `src`.

Re-raising (`raise`), `_thread.interrupt_main()`, and an `isinstance(exc, KeyboardInterrupt)` dispatch inside a broad handler all satisfy the rule — what it forbids is *swallowing* an interrupt, because one that never reaches the main thread is one the user cannot deliver. A deliberate exception (a CLI's top-level handler, where Ctrl+C is the documented way to quit) is marked `# noqa: KBI002` with a comment saying why; a bare `# noqa` will not silence it.

Run it alone with:
```bash
uv run --no-sync python -m ci.lint_python.keyboard_interrupt_checker src --exclude .venv venv dist .build
```

**Bincode forbidden**: `disallowed_methods = "deny"` is wired through `clippy.toml` at the workspace root — every member crate refuses bincode serialization (broker wire stays prost-only). Phase 0 of #228.

## Code Quality Notes

- **Complex Functions** (refactor if modifying): `ProcessOutputReader.run()` (C12), `RunningProcess.get_next_line()` (C16), `RunningProcess.wait()` (C20)
- **Print Statements**: Console output via print() is intentional for CLI functionality
- **Exception Handling**: Broad exception handling is acceptable for process cleanup/recovery scenarios
- **Cross-Platform**: Code must work on Windows (MSYS), macOS, and Linux

## Workspace Config

- Rust edition 2021, version 1.85+, shared workspace dependencies: `pyo3 0.29`, `rusqlite 0.32` (bundled), `thiserror 2`
- Python requires >= 3.10, uses ABI3 stable API (`abi3-py310`)
- Release profile: line-tables-only debug info for workspace members, no debug
  info for dependencies (`[profile.release.package."*"] debug = false`), no
  stripping. The line tables are what let a release-mode stack resolve to
  `file:line` in this crate's own frames (#803); dependencies are excluded
  because nobody reads their DWARF and it costs build time and disk.
  (This line previously claimed "packed split-debuginfo", which was never in
  the manifest.)
