# running-process

[![PyPI](https://img.shields.io/pypi/v/running-process)](https://pypi.org/project/running-process/) [![Crates.io](https://img.shields.io/crates/v/running-process)](https://crates.io/crates/running-process) [![codecov](https://codecov.io/gh/zackees/running-process/graph/badge.svg)](https://codecov.io/gh/zackees/running-process) [![Dependency Check](https://github.com/zackees/running-process/actions/workflows/dependency-check.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/dependency-check.yml)

`running-process` is what you wished python's subprocess was. Blazing fast, highly concurrent, huge feature list, dead process tracking, pty support. Built in Rust with a thin python api.

## v1 Broker Docs

The v1 broker work is documented as a stable spec alongside the implementation:

- Core spec: [architecture](docs/v1-architecture-overview.md), [frozen commitments](docs/v1-frozen-commitments.md), [pipe naming](docs/v1-pipe-naming.md), [platform behavior](docs/v1-platform-behavior.md), [security model](docs/v1-security-model.md)
- Schemas: [wire envelope](docs/v1-wire-envelope.md), [cache manifest](docs/v1-cache-manifest.md), [service definition](docs/v1-service-definition.md), [lifecycle events](docs/v1-lifecycle-events.md)
- Consumer adoption: [dashboard](docs/v1-consumer-adoption-dashboard.md), [clud](docs/consumer-adoption-clud.md), [zccache](docs/consumer-adoption-zccache.md), [soldr](docs/consumer-adoption-soldr.md), [fbuild](docs/consumer-adoption-fbuild.md)
- Operations: [broker architecture](docs/v1-broker-architecture.md), [admin verbs](docs/v1-admin-verbs.md), [backend lifecycle](docs/v1-backend-lifecycle.md), [handoff optimization](docs/v1-handoff-optimization.md), [observability](docs/v1-observability.md)
- Rollout: [policy](docs/v1-rollout-policy.md), [escape hatch](docs/v1-escape-hatch.md), [troubleshooting](docs/v1-troubleshooting.md)
- Examples: [minimal consumer](examples/minimal-consumer/), [release-handles CLI](examples/release-handles-cli/), [custom isolation](examples/custom-isolation/)
- Contrib service templates: [systemd](contrib/systemd/running-process-broker-v1.service), [launchd](contrib/launchd/com.zackees.running-process-broker-v1.plist), [Windows service installer](contrib/windows-service/install.ps1)

| Platform | CI |
|----------|----|
| Linux | [![Linux](https://github.com/zackees/running-process/actions/workflows/ci-linux.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/ci-linux.yml) |
| macOS | [![macOS](https://github.com/zackees/running-process/actions/workflows/ci-macos.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/ci-macos.yml) |
| Windows | [![Windows](https://github.com/zackees/running-process/actions/workflows/ci-windows.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/ci-windows.yml) |
| Release | [![Auto Release](https://github.com/zackees/running-process/actions/workflows/auto-release.yml/badge.svg)](https://github.com/zackees/running-process/actions/workflows/auto-release.yml) |

Each `ci-*` workflow fans out to that platform's x86 and ARM lanes (and
musl on Linux), running build, lint, unit and integration stages per lane.




## Why?

This project started off as a fix for python's sub process module. It was in python originally, but then moved to OS specific rust. Now it's blazing fast: using OS threads, atomics and proper signaling back to the python api. This library also allows stderr and stdout stream reading in parallel, something `subprocess` lacks. It also has cross platform process tracking, pty generation. It has zombie process tracking. It also has builtin `expect` for keyword event triggers, `idle tracking` (great for agent CLI's that dont' notifiy when they are done, they just stop sending data).

This libary is design for speed and correctness and portability. Usually terminal utilities are for windows or linux/mac. This is designed to run everywhere.

## PTY Support Matrix

PTY support is a guaranteed part of the package contract on:

- Windows
- Linux
- macOS

On those platforms, `RunningProcess.pseudo_terminal(...)`, `wait_for_expect(...)`, and `wait_for_idle(...)` are core functionality rather than optional extras.

`Pty.is_available()` remains as a compatibility shim and only reports `False` on unsupported platforms.

### Windows 10 ConPTY sidecar

`running-process` enables `PSEUDOCONSOLE_PASSTHROUGH_MODE` on Windows so the
master pipe receives the child's raw ANSI bytes instead of conhost's
synthesized re-emission. The flag is only honored natively on Windows 11 /
Server 2022 (build 22000+). On Windows 10, Microsoft's official answer is the
[`Microsoft.Windows.Console.ConPTY` NuGet redistributable][nuget-conpty] — a
paired `conpty.dll` + `OpenConsole.exe` that intercepts `CreatePseudoConsole`
and runs a modern OpenConsole instance instead of the system conhost.

**This is transparent. Consumers do nothing.** On first ConPTY use:

- **Windows 11+** → `kernel32!CreatePseudoConsole` directly. No extra files,
  no network, no behavior change.
- **Windows 10, cache hit** → load `conpty.dll` from the platform cache
  (`%LOCALAPPDATA%\running-process\conpty\<rp-version>\<arch>\` by default).
  Silent.
- **Windows 10, cache miss** → one-time HTTPS fetch of
  `conpty-sidecar-<arch>.tar.zst` (a few MB, zstd-19) from this crate's
  matching GitHub release, decompressed atomically into the cache, then
  loaded. Subsequent runs are silent cache hits.
- **Windows 10, fetch failure** (no network, firewall, offline) → fall back
  to `kernel32` (legacy virtual-screen renderer) with a one-line warning. No
  crash.

Trust model: HTTPS to github.com plus GitHub's content-locked release assets
mean an attacker who could substitute the sidecar could also substitute the
crate itself. The runtime additionally SHA-256 verifies the downloaded bytes
against a per-arch hash baked into the crate at compile time (see Integrity
below) — defense-in-depth on top of HTTPS.

Pre-staging for air-gapped hosts: drop `conpty.dll` + `OpenConsole.exe` next
to the host executable. That path is checked before the cache and before any
network access, so air-gapped deployments never need network.

#### Integrity

Each fetched sidecar asset has its SHA-256 baked into the crate at compile time
from `conpty-sidecar.sha256.toml` (the same manifest the release workflow
uploads alongside the tarballs). The runtime compares the downloaded bytes
against that hash before decompressing — a mismatch logs both hashes
(`RUNNING_PROCESS_CONPTY_DIAGNOSTICS=1` shows it without the `kernel32`
fallback noise) and falls back to `kernel32`. This catches a mirror or
caching proxy corrupting bytes in flight and tightens the audit story when
an operator wants to compare runtime behavior against the published
`SHA256SUMS` for the release.

Pre-release dev checkouts ship with an empty manifest, so the runtime skips
verification with a one-line `[diag] no expected sha for arch <arch>;
downloading without verification` log line. The release workflow rewrites
the manifest before wheels and the crate are built so each shipped artifact
bakes in hashes matching the tarballs uploaded to that same release (#447).

Env vars:

- `RUNNING_PROCESS_USE_SYSTEM_CONPTY=1` — force `kernel32` even on Windows 10.
  Skip the sidecar and the fetch entirely. Escape hatch.
- `RUNNING_PROCESS_CONPTY_OFFLINE=1` — never attempt the network fetch. The
  library still uses a pre-staged sidecar (next to the exe) or the existing
  cache; otherwise it falls back to `kernel32`. Use this in build sandboxes
  and air-gapped runners.
- `RUNNING_PROCESS_CONPTY_CACHE=<dir>` — override the sidecar cache root.
- `RUNNING_PROCESS_CONPTY_DIAGNOSTICS=1` — log the selected backend, the
  cache decision, and the detected Windows build on first ConPTY use.

Release assets only exist for `running-process` versions published after the
self-acquisition feature shipped. On an older version, Win10 hosts always
fall back to `kernel32` unless a pre-staged sidecar is present.

[nuget-conpty]: https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY/

## Terminal Graphics Capabilities

Rust callers can inspect terminal graphics support with
`running_process::current_terminal_capabilities()` or the pure
`running_process::detect_terminal_capabilities(...)` helper. The result reports
Sixel, Kitty graphics, and iTerm2 `File=` image support as structured
capability records with `status`, `evidence`, `source`, and `risks` metadata.

The detector intentionally distinguishes terminal hosts from shells. `cmd.exe`,
PowerShell, Git Bash, bash, zsh, and fish are command interpreters; they do not
prove graphics support. The terminal host or multiplexer does: Windows
Terminal, xterm, foot, Konsole, WezTerm, Kitty, iTerm2, tmux, GNU screen, and
similar programs provide the relevant evidence. Weak aliases such as
`TERM=xterm-256color` are reported as unknown unless a live probe or stronger
host signal confirms support.

## CLI Helpers

The package installs a `running-process` wrapper CLI for supervised command execution:

```bash
running-process --timeout 30 -- python -m pytest tests/test_cli.py
running-process --find-leaks -- python worker.py
```

`--find-leaks` tags the wrapped process tree with a unique originator marker and reports any
descendants still alive after the direct child exits.

## Cleanup Manifests

The `running-process-cleanup` binary reads v1 broker `CacheManifest` files
without requiring a broker or daemon to be running. Manifests are written in two
places:

- each daemon cache root: `.running-process-manifest.pb`
- the central registry: `$XDG_DATA_HOME/running-process/manifests/` on Linux,
  `~/Library/Application Support/running-process/manifests/` on macOS, and
  `%APPDATA%\running-process\manifests\` on Windows

Destructive commands are dry-run by default. Add `--confirm` to delete selected
roots:

```bash
running-process-cleanup list --json
running-process-cleanup verify --json
running-process-cleanup prune --dormant-after 30d
running-process-cleanup prune --dormant-after 30d --confirm
running-process-cleanup prune --keep-current --keep-last 2
running-process-cleanup uninstall zccache --keep-config
running-process-cleanup instances --json
```

For GitHub Actions cache restores, run verification after `actions/cache@v4`
restores daemon state. Manifests from a prior runner boot are reported as stale:

```yaml
- uses: actions/cache@v4
  with:
    path: ~/.local/share/running-process
    key: running-process-${{ runner.os }}-${{ hashFiles('**/Cargo.lock') }}

- name: Verify restored running-process manifests
  run: running-process-cleanup verify --json
```

## Pipe-backed API

```python
from running_process import RunningProcess

process = RunningProcess(
    ["python", "-c", "import sys; print('out'); print('err', file=sys.stderr)"]
)

process.wait()

print(process.stdout)          # stdout only
print(process.stderr)          # stderr only
print(process.combined_output) # combined compatibility view
```

Captured data values stay plain `str | bytes`. Live stream handles are exposed separately:

```python
if process.stdout_stream.available():
    print(process.stdout_stream.drain())
```

Process priority is a first-class launch option:

```python
from running_process import CpuPriority, RunningProcess

process = RunningProcess(
    ["python", "-c", "import time; time.sleep(1)"],
    nice=CpuPriority.LOW,
)
```

`nice=` behavior:

- accepts either a raw `int` niceness or a platform-neutral `CpuPriority`
- on Unix, it maps directly to process niceness
- on Windows, positive values map to below-normal or idle priority classes and negative values map to above-normal or high priority classes
- `0` leaves the default scheduler priority unchanged
- positive values are the portable default; negative values may require elevated privileges
- the enum intentionally stops at `HIGH`; there is no realtime tier

Available helpers:

- `get_next_stdout_line(timeout)`
- `get_next_stderr_line(timeout)`
- `get_next_line(timeout)` for combined compatibility reads
- `stream_iter(timeout)` or `for stdout, stderr, exit_code in process`
- `drain_stdout()`
- `drain_stderr()`
- `drain_combined()`
- `stdout_stream.available()`
- `stderr_stream.available()`
- `combined_stream.available()`

`stream_iter(...)` yields tuple-like `ProcessOutputEvent(stdout, stderr, exit_code)` records.
Only one stream payload is populated per nonterminal item. When both pipes are drained, it yields
`(EOS, EOS, exit_code)` if the child has already exited, or `(EOS, EOS, None)` followed by a final
`(EOS, EOS, exit_code)` if the child closed both pipes before it exited.

`RunningProcess.run(...)` supports common `subprocess.run(...)` style cases including:

- `capture_output=True`
- `text=True`
- `encoding=...`
- `errors=...`
- `shell=True`
- `env=...`
- `nice=...`
- `stdin=subprocess.DEVNULL`
- `input=...` in text or bytes form

Unsupported `subprocess.run(...)` kwargs now fail loudly instead of being silently ignored.

## Expect API

`expect(...)` is available on both the pipe-backed and PTY-backed process APIs.

```python
import re
import subprocess
from running_process import RunningProcess

process = RunningProcess(
    ["python", "-c", "print('prompt>'); import sys; print('echo:' + sys.stdin.readline().strip())"],
    stdin=subprocess.PIPE,
)

process.expect("prompt>", timeout=5, action="hello\n")
match = process.expect(re.compile(r"echo:(.+)"), timeout=5)
print(match.groups)
```

Supported `action=` forms:

- `str` or `bytes`: write to stdin
- `"interrupt"`: send Ctrl-C style interrupt when supported
- `"terminate"`
- `"kill"`

Pipe-backed `expect(...)` matches line-delimited output. If the child writes prompts without trailing newlines, use the PTY API instead.

## PTY API

Use `RunningProcess.pseudo_terminal(...)` for interactive terminal sessions. It is chunk-oriented by design and preserves carriage returns and terminal control flow instead of normalizing it away.

```python
from running_process import ExpectRule, RunningProcess

pty = RunningProcess.pseudo_terminal(
    ["python", "-c", "import sys; sys.stdout.write('name?'); sys.stdout.flush(); print('hello ' + sys.stdin.readline().strip())"],
    text=True,
    expect=[ExpectRule("name?", "world\n")],
    expect_timeout=5,
)

print(pty.output)
```

PTY behavior:

- accepts `str` and `list[str]` commands
- auto-splits simple string commands into argv when shell syntax is not present
- uses shell mode automatically when shell metacharacters are present
- is guaranteed on supported Windows, Linux, and macOS builds
- keeps output chunk-buffered by default
- preserves `\r` for redraw-style terminal output
- supports `write(...)`, `read(...)`, `drain()`, `available()`, `expect(...)`, `resize(...)`, and `send_interrupt()`
- supports `nice=...` at launch
- supports `interrupt_and_wait(...)` for staged interrupt escalation
- supports `wait_for_idle(...)` with activity filtering
- exposes `exit_reason`, `interrupt_count`, `interrupted_by_caller`, and `exit_status`

`wait_for_idle(...)` has two modes:

- default fast path: built-in PTY activity rules and optional process metrics
- slow path: `IdleDetection(idle_reached=...)`, where your Python callback receives an `IdleInfoDiff` delta and returns `IdleDecision.DEFAULT`, `IdleDecision.ACTIVE`, `IdleDecision.BEGIN_IDLE`, or `IdleDecision.IS_IDLE`

There is also a compatibility alias: `RunningProcess.psuedo_terminal(...)`.

Rust consumers should make the same transport choice explicitly: use
`NativeProcess` for one-shot noninteractive work and
`InteractivePtySession` / `NativePtyProcess` only for real terminal sessions.
See [Rust PTY guidance](docs/RUST_PTY.md).

You can also inspect the intended interactive launch semantics without launching a child:

```python
from running_process import RunningProcess

spec = RunningProcess.interactive_launch_spec("console_isolated")
print(spec.ctrl_c_owner)
print(spec.creationflags)
```

Supported launch specs:

- `pseudo_terminal`
- `console_shared`
- `console_isolated`

For an actual launch, use `RunningProcess.interactive(...)`:

```python
process = RunningProcess.interactive(
    ["python", "-c", "print('hello from interactive mode')"],
    mode="console_shared",
    nice=5,
)
process.wait()
```

## Abnormal Exits

By default, nonzero exits stay subprocess-like: you get a return code and can inspect `exit_status`.

```python
process = RunningProcess(["python", "-c", "import sys; sys.exit(3)"])
process.wait()
print(process.exit_status)
```

If you want abnormal exits to raise, opt in:

```python
from running_process import ProcessAbnormalExit, RunningProcess

try:
    RunningProcess.run(
        ["python", "-c", "import sys; sys.exit(3)"],
        capture_output=True,
        raise_on_abnormal_exit=True,
    )
except ProcessAbnormalExit as exc:
    print(exc.status.summary)
```

Notes:

- keyboard interrupts still raise `KeyboardInterrupt`
- `kill -9` / `SIGKILL` is classified as an abnormal signal exit
- possible OOM conditions are exposed as a hint on `exit_status.possible_oom`
- OOM cannot be identified perfectly across platforms from exit status alone, so it is best-effort rather than guaranteed

## Text and bytes

Pipe mode is byte-safe internally:

- invalid UTF-8 does not break capture
- text mode decodes with UTF-8 and `errors="replace"` by default
- binary mode returns bytes unchanged
- `\r\n` is normalized as a line break in pipe mode
- bare `\r` is preserved

PTY mode is intentionally more conservative:

- output is handled as chunks, not lines
- redraw-oriented `\r` is preserved
- no automatic terminal-output normalization is applied

## Development

```bash
./install
./lint
./test
```

`./install` bootstraps `rustup` into the shared user locations (`~/.cargo` and `~/.rustup`, or `CARGO_HOME` / `RUSTUP_HOME` if you override them), then installs the exact toolchain pinned in `rust-toolchain.toml`. Toolchain installs are serialized with a lock so concurrent repo bootstraps do not race the same shared version. Rust build commands run through `uvx soldr`, so there is no separate `soldr` install step to maintain.

`./lint` applies `cargo fmt` and Ruff autofixes before running the remaining lint checks, so fixable issues are rewritten in place.

`./test` runs the Rust tests, rebuilds the native extension with the unoptimized `dev` profile, runs the non-live Python tests, and then runs the `@pytest.mark.live` coverage that exercises real OS process and signal behavior.

### Repository Dylint

The native Linux x86 consolidated preflight runs the repository-owned
`running_process_env_literal` lint. It rejects direct string literals for
`RUNNING_PROCESS_*` environment controls in `std::env` calls, keeping broker
escape hatches and test seams tied to their canonical constants. The lint
library, `cargo-dylint`, and `dylint-link` are pinned to Dylint `6.0.1`; the
library and CI both use `nightly-2026-04-16`.

To run the same gate locally:

```bash
soldr rustup toolchain install nightly-2026-04-16 --profile minimal \
  --component rustc-dev --component llvm-tools-preview
uvx soldr cargo install cargo-dylint@6.0.1 dylint-link@6.0.1 --locked
(
  cd lints/running-process-env-literal
  CARGO_TARGET_DIR=../../target/dylint RUSTUP_TOOLCHAIN=nightly-2026-04-16 \
    uvx soldr cargo test --locked
)
CARGO_TARGET_DIR=target/dylint RUSTUP_TOOLCHAIN=nightly-2026-04-16 \
  uvx soldr cargo dylint --all --workspace
```

The first check exercises the negative UI fixture and proves the lint rejects
a literal control name. The second checks the workspace. CI gives the exact
tool binaries and nightly target directory dedicated persistent caches and
runs the gate once, rather than adding a cold Dylint build to every platform
job.

On local developer machines, `./test` also runs the Linux Docker preflight so Windows and macOS development catches Linux wheel, lint, and non-live pytest regressions before push. GitHub-hosted Actions skip that Docker-only preflight and run the native platform suite directly.

For a live-only test run with the timeout crash watchdog and automatic thread
dumps still enabled, use:

```bash
uv run -m ci.test --live-only
```

For a narrower live-only selection, pass pytest targets and selectors through
the same entrypoint:

```bash
uv run -m ci.test --live-only tests/test_pty_support.py interrupt
```

For direct Cargo build commands, use `uvx soldr` directly:

```bash
uvx soldr cargo check --workspace
uvx soldr cargo test --workspace
uvx soldr cargo package -p running-process --no-verify
```

Keep `maturin`, `cargo fmt`, and `cargo clippy` on their normal entrypoints.
This repo's high-level scripts already choose the compatible path for those
tools.

On Windows, native rebuilds that compile bundled C code should run from a Visual Studio developer shell. When the environment is ambiguous, point `maturin` at the MSVC toolchain binaries directly rather than relying on the generic cargo proxy.

For local extension rebuilds, prefer:

```bash
uv run build.py
```

That defaults to building a dev-profile wheel and reinstalling it into the repo's `uv` environment, which keeps the native extension in `site-packages` instead of copying it into `src/`. For publish-grade artifacts, use:

```bash
uv run build.py --release
```

## Release

Releases are cut by the **Auto Release** GitHub Actions workflow. Bump `project.version` in `pyproject.toml` (and match `workspace.package.version` in `Cargo.toml`), push the commit to `main`, and the workflow will:

- Build wheels for linux x86/arm, macOS x86/arm, and Windows x86/arm and publish them to PyPI via trusted publishing.
- Publish `running-process-{proto, core, client, py}` to crates.io in dependency order (requires the repo secret `CARGO_REGISTRY_TOKEN`).
- Build standalone `runpm` and `running-process-daemon` binaries for each target and attach them — alongside the wheels, `install.sh`, `install.ps1`, and `SHA256SUMS` — to a new GitHub Release.

You can also fire the workflow manually with `gh workflow run auto-release.yml`, or by pushing a `vX.Y.Z` tag.

The standalone binaries can be installed without `pip`:

```bash
curl -LsSf https://github.com/zackees/running-process/releases/latest/download/install.sh | sh
```

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/zackees/running-process/releases/latest/download/install.ps1 | iex"
```

## Process Containment

`ContainedProcessGroup` ensures all child processes are killed when the group is dropped, using OS-level mechanisms (Job Objects on Windows, process groups + `SIGKILL` on Unix).

```python
from running_process import ContainedProcessGroup

with ContainedProcessGroup() as group:
    proc = group.spawn(["sleep", "3600"])
# all children killed on exit, even on crash
```

### Crash-resilient orphan discovery

When a parent crashes, its in-process registry is lost. `ContainedProcessGroup` can stamp every child with an environment variable that survives parent death:

```python
from running_process import ContainedProcessGroup, find_processes_by_originator

# At launch: tag children with your tool name
with ContainedProcessGroup(originator="MYTOOL") as group:
    proc = group.spawn(["long-running-worker"])

# Later (from any process, any session): find orphans
stale = find_processes_by_originator("MYTOOL")
for info in stale:
    if not info.parent_alive:
        print(f"Orphaned PID {info.pid} from dead parent {info.parent_pid}")
```

The env var `RUNNING_PROCESS_ORIGINATOR=TOOL:PID` is inherited by all descendants. The scanner uses process start times to guard against PID reuse.

## Detached Launches

Use `launch_detached(...)` when a caller needs to start a daemon-tracked shell command and return immediately:

```python
from running_process import launch_detached

handle = launch_detached(
    "python worker.py",
    cwd=".",
    env={"WORKER_MODE": "background"},
    originator="mytool:session-1",
)
print(handle.pid)
```

This path uses the running-process daemon for launch/tracking. It is separate from `running_process.daemon.spawn_daemon(...)`, which keeps the trampoline-based process-name behavior.

## Tracked PID Cleanup

`RunningProcess`, `InteractiveProcess`, and PTY-backed launches register their live PIDs in a SQLite database. The default location is:

- Windows: `%LOCALAPPDATA%\\running-process\\tracked-pids.sqlite3`
- Override: `RUNNING_PROCESS_PID_DB=/custom/path/tracked-pids.sqlite3`

If a bad run leaves child processes behind, terminate everything still tracked in the database:

```bash
python scripts/terminate_tracked_processes.py
```

## Notes

- `stdout` and `stderr` are no longer merged by default.
- `combined_output` exists for compatibility when you need the merged view.
- `RunningProcess(..., use_pty=True)` is no longer the preferred path; use `RunningProcess.pseudo_terminal(...)` for PTY sessions.
- On supported Windows builds, PTY support is provided by the native Rust extension rather than a Python `winpty` fallback.
- The test suite checks that `running_process.__version__`, package metadata, and manifest versions stay in sync.
