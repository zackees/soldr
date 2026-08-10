# Terminal Inspector Plan

## Goal

Add a Rust-owned terminal inspection feature that can answer:

- what terminal-backed processes exist on the system right now
- which of them were spawned by `running-process`
- which host terminal they came from
- which ones are zombies, orphaned, or otherwise suspicious
- how much memory and buffered output each tracked process is holding
- which runtime objects and reader threads still exist after child exit

This feature is for leak investigation first, not for public API polish first.

## Why The Current State Is Not Enough

The current code can only partially inspect processes created by this library, and even that visibility is incomplete.

### Current blind spots

- The Python manager only tracks live wrapper objects via a `WeakSet`.
- There is no Rust-side registry of spawned processes or PTYs.
- Output history is stored unbounded in memory for both pipe and PTY paths.
- Reader threads are detached and not represented in any durable lifecycle record.
- There is no system-wide census of terminal hosts or zombie descendants.

### Existing leak-sensitive code

- Pipe output history is unbounded in `crates/running-process/src/lib.rs`.
- PTY output history is unbounded in `crates/running-process-py/src/lib.rs`.
- Reader threads are spawned in Rust and then become opaque to diagnostics.
- Python-side active process tracking is object-lifetime-based rather than process-lifetime-based.

## Non-Goals

- perfect tab-level attribution inside every terminal emulator on day one
- complete forensic reconstruction of arbitrary non-child process trees
- replacing `psutil`-based Python helpers immediately
- a stable long-term public API in the first milestone

The first milestone is an investigative tool for leak hunting.

## Product Shape

Implement two correlated inspectors.

### 1. Runtime registry

This tracks every process and PTY started by the Rust runtime.

Purpose:

- show what our library thinks still exists
- measure retained buffers and reader state
- catch mismatches between runtime state and OS state

### 2. System terminal census

This scans the OS for terminal-related processes whether we created them or not.

Purpose:

- find zombie terminals and orphaned descendants
- identify terminal hosts such as `conhost`, `OpenConsole`, `WindowsTerminal`, shells, and PTY children
- correlate suspicious system processes back to our runtime registry where possible

## Architecture

### Rust crate ownership

The feature should live primarily in `running-process`.

Suggested modules:

- `inspector/mod.rs`
- `inspector/runtime_registry.rs`
- `inspector/system_scan.rs`
- `inspector/types.rs`
- `inspector/heuristics.rs`
- `inspector/json.rs`

The PyO3 crate should only expose the resulting snapshots and helper methods.

### Core rule

All authoritative leak-state bookkeeping should happen in Rust, not in Python wrapper objects.

## Data Model

### Runtime records

Add a registry entry for every process started through:

- `NativeProcess`
- `NativePtyProcess`

Suggested structs:

```rust
pub struct RuntimeId(pub u64);

pub enum RuntimeKind {
    PipeProcess,
    PtyProcess,
    InteractiveShared,
    InteractiveIsolated,
}

pub enum RuntimeState {
    Starting,
    Running,
    Exited { code: i32 },
    KillRequested,
    Closed,
    Lost,
}

pub struct BufferStats {
    pub queued_bytes: u64,
    pub history_bytes: u64,
    pub dropped_bytes: u64,
    pub line_count: u64,
    pub chunk_count: u64,
}

pub struct ThreadStats {
    pub reader_threads_spawned: u32,
    pub reader_threads_alive: u32,
    pub watcher_threads_spawned: u32,
    pub watcher_threads_alive: u32,
}

pub struct RuntimeProcessRecord {
    pub runtime_id: RuntimeId,
    pub kind: RuntimeKind,
    pub pid: Option<u32>,
    pub parent_pid: Option<u32>,
    pub command: Vec<String>,
    pub shell_wrapped: bool,
    pub cwd: Option<String>,
    pub created_at_unix_ms: u128,
    pub started_at_unix_ms: Option<u128>,
    pub exited_at_unix_ms: Option<u128>,
    pub returncode: Option<i32>,
    pub runtime_state: RuntimeState,
    pub stdin_mode: String,
    pub capture_enabled: bool,
    pub stdout: BufferStats,
    pub stderr: BufferStats,
    pub combined: BufferStats,
    pub pty: Option<BufferStats>,
    pub last_input_at_unix_ms: Option<u128>,
    pub last_output_at_unix_ms: Option<u128>,
    pub threads: ThreadStats,
    pub notes: Vec<String>,
}
```

### System records

Suggested structs:

```rust
pub enum ProcessState {
    Running,
    Sleeping,
    Stopped,
    Zombie,
    Dead,
    Unknown,
}

pub enum TerminalHostKind {
    Conhost,
    OpenConsole,
    WindowsTerminal,
    WezTerm,
    Tmux,
    Screen,
    GnomeTerminal,
    Alacritty,
    TerminalApp,
    Shell,
    Unknown,
}

pub enum TerminalOrigin {
    SpawnedByUs { runtime_id: u64 },
    DescendedFromRuntime { runtime_id: u64, ancestor_pid: u32 },
    SpawnedByTerminalHost { host_pid: u32, host_kind: TerminalHostKind },
    Orphaned { previous_parent_pid: Option<u32> },
    Unknown,
}

pub struct SystemTerminalRecord {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub process_group_id: Option<u32>,
    pub session_id: Option<u32>,
    pub tty_name: Option<String>,
    pub image_name: String,
    pub command_line: Option<String>,
    pub cwd: Option<String>,
    pub state: ProcessState,
    pub started_at_unix_ms: Option<u128>,
    pub rss_bytes: Option<u64>,
    pub virtual_bytes: Option<u64>,
    pub handle_count: Option<u64>,
    pub fd_count: Option<u64>,
    pub thread_count: Option<u32>,
    pub host_kind: TerminalHostKind,
    pub origin: TerminalOrigin,
    pub child_pids: Vec<u32>,
    pub notes: Vec<String>,
}
```

### Leak findings

Suggested output:

```rust
pub enum LeakSeverity {
    Critical,
    High,
    Medium,
    Low,
}

pub struct TerminalLeakFinding {
    pub severity: LeakSeverity,
    pub pid: Option<u32>,
    pub runtime_id: Option<u64>,
    pub summary: String,
    pub evidence: Vec<String>,
}

pub struct InspectorSnapshot {
    pub collected_at_unix_ms: u128,
    pub runtime_records: Vec<RuntimeProcessRecord>,
    pub system_terminals: Vec<SystemTerminalRecord>,
    pub findings: Vec<TerminalLeakFinding>,
}
```

## Runtime Registry Design

### Requirements

- register every process before `spawn`
- update with actual PID immediately after `spawn`
- update on `poll`, `wait`, `kill`, `terminate`, `close`, and `drop`
- keep exited records for a configurable retention period
- never depend on Python object lifetime for truth

### Ownership model

Use a global Rust registry behind `OnceLock<Arc<Registry>>`.

Suggested internal shape:

```rust
struct Registry {
    next_runtime_id: AtomicU64,
    records: RwLock<HashMap<RuntimeId, Arc<Mutex<RuntimeProcessRecord>>>>,
    pid_index: RwLock<HashMap<u32, RuntimeId>>,
}
```

### Retention policy

Do not delete records immediately on exit. Keep them for investigation.

Suggested default:

- active records: always retained
- exited records: retained for 10 minutes
- closed records: retained for 10 minutes
- explicit purge API for tests

### Buffer accounting

The current full-history behavior is itself a leak risk. The inspector must report memory accurately even if history stays configurable.

Add configurable capture policy:

```rust
pub struct CapturePolicy {
    pub max_history_bytes_per_stream: usize,
    pub max_queue_bytes_per_stream: usize,
    pub max_combined_history_bytes: usize,
    pub truncate_oldest: bool,
}
```

Recommended first implementation:

- keep existing behavior behind compatibility defaults
- add byte counters immediately
- then switch history storage to ring buffers

Reported fields must include:

- current retained bytes
- dropped bytes due to truncation
- line or chunk count

### Reader thread visibility

Detached threads are not diagnosable enough.

Add thread lifecycle markers:

- reader thread spawned
- reader thread started
- reader thread observed EOF
- reader thread terminated

Do not try to kill threads directly. Record their lifecycle and infer stale threads if they remain alive after child exit beyond a threshold.

## System Collector Design

The system collector should use direct OS data where possible and only use third-party crates to reduce boilerplate.

### Dependency recommendation

Use:

- `sysinfo` for cross-platform baseline process enumeration

Platform-specific augmentation:

- Unix: direct `/proc` parsing where `sysinfo` is missing state like zombie or TTY details
- Windows: Win32 APIs for command line, session id, handle count, and host process identity

Do not rely on `sysinfo` alone for zombie attribution.

### Unix collection

Collect from:

- `/proc/<pid>/stat`
- `/proc/<pid>/status`
- `/proc/<pid>/cmdline`
- `/proc/<pid>/cwd`
- `/proc/<pid>/fd`

Fields to derive:

- pid, ppid
- process group id
- session id
- state including zombie
- tty number and tty name if resolvable
- fd count
- command line
- cwd
- rss

Terminal heuristics:

- process has controlling TTY
- process name matches shell or terminal multiplexer
- process is under a PTY slave
- ancestor chain includes terminal emulator or multiplexer

### Windows collection

Collect from:

- Toolhelp or `NtQuerySystemInformation` for process list
- process handle inspection for creation time and counts
- command line retrieval via documented or stable low-level query path
- session id via process APIs

Fields to derive:

- pid, ppid
- process image name
- command line
- creation time
- rss / private bytes if available
- handle count
- thread count
- session id

Terminal heuristics:

- `conhost.exe`
- `OpenConsole.exe`
- `WindowsTerminal.exe`
- `wezterm-gui.exe`
- shells such as `cmd.exe`, `powershell.exe`, `pwsh.exe`, `bash.exe`, `wsl.exe`
- descendants of those processes

ConPTY note:

Exact tab attribution inside Windows Terminal is not required in the first milestone. Parent and ancestor correlation is sufficient.

## Correlation Strategy

Correlate runtime records to system records using:

- PID
- process start time when available

Never match on PID alone without considering reuse for long-lived registries.

Suggested correlation flow:

1. build system process map
2. mark exact PID plus start-time matches as `SpawnedByUs`
3. walk descendants of known runtime PIDs and mark as `DescendedFromRuntime`
4. classify remaining terminal-related processes by host ancestry

## Zombie And Leak Heuristics

### Critical findings

- OS process state is zombie
- runtime marks process active but OS says exited or dead
- PTY or pipe reader thread still alive long after child exit
- runtime object closed but descendant terminal child remains alive

### High findings

- retained output buffers exceed threshold
- terminal host remains alive with no meaningful foreground child
- repeated snapshots show monotonic RSS growth while process is idle
- repeated snapshots show monotonic handle or fd growth while process is idle

### Medium findings

- parent is gone and child was originally ours
- process sits in stopped state with no associated runtime handle
- runtime registry contains stale records with no matching PID and no exit timestamp

### Low findings

- shell wrapper remains alive longer than child workload would suggest
- control process exists with no recent I/O and stable memory

## Snapshot And Diff Mode

One-time inspection is not enough for leak work. Add repeated sampling support.

Suggested APIs:

```rust
pub fn collect_snapshot() -> InspectorSnapshot;
pub fn collect_snapshot_json() -> String;
pub fn diff_snapshots(old: &InspectorSnapshot, new: &InspectorSnapshot) -> SnapshotDiff;
```

Diffs should report:

- new PIDs
- disappeared PIDs
- runtime records that changed state
- rss delta
- handle or fd delta
- buffer-byte delta
- reader-thread state changes

## PyO3 Surface

Expose minimal APIs first.

Suggested methods in `_native`:

- `inspect_runtime_processes() -> list[dict]`
- `inspect_system_terminals() -> list[dict]`
- `inspect_terminals() -> dict`
- `inspect_terminals_json() -> str`
- `purge_runtime_registry() -> None`

Python wrapper layer can later provide:

- pretty-print report
- filtering by PID
- repeated sampling helper
- warn-on-findings helper for tests

## CLI / Debug Workflow

Add a developer-facing helper script later, but keep the source of truth in Rust.

Suggested future command:

```text
python -m running_process.debug.terminals --json
python -m running_process.debug.terminals --watch 1.0
python -m running_process.debug.terminals --only-zombies
```

## Phased Delivery

### Phase 1: Runtime registry

Deliver:

- Rust global registry
- runtime process and PTY registration
- buffer-byte accounting
- reader-thread lifecycle accounting
- JSON snapshot for runtime-owned processes only

Success criteria:

- we can see every child we spawn
- we can identify retained output memory per process
- we can identify children that exited but still have live reader state

### Phase 2: System census

Deliver:

- cross-platform system process enumeration
- terminal-host classification
- zombie detection
- ancestry correlation back to runtime records

Success criteria:

- we can list all terminal-related processes on the system
- we can distinguish ours from not-ours
- we can flag zombie or orphaned descendants

### Phase 3: Snapshot diffing

Deliver:

- repeated sampling API
- delta computation
- growth heuristics for memory, handles, and retained buffers

Success criteria:

- we can identify what grows over time rather than guessing

### Phase 4: Capture-policy hardening

Deliver:

- configurable bounded history buffers
- dropped-byte counters
- defaults that avoid unbounded memory growth

Success criteria:

- instrumentation itself does not become the leak

## Testing Plan

### Rust tests

- registry records process start, exit, and close
- PID reuse does not misattribute records
- reader thread lifecycle updates correctly
- buffer-byte counters match emitted data
- bounded buffer truncation updates dropped-byte counters

### Integration tests

- pipe child exits cleanly and disappears from active runtime state
- PTY child exits cleanly and reader thread terminates
- killed PTY child is reported with correct origin and exit state
- intentionally leaked child remains visible in findings

### Platform-specific tests

Unix:

- zombie child is detected using `/proc`
- PTY slave or controlling TTY mapping is populated when available

Windows:

- `conhost` or `OpenConsole` ancestry is visible for PTY-backed processes
- shell and terminal host classification is correct for known spawned examples

## Recommended Immediate Implementation Order

1. Add `runtime_registry.rs` and wire `NativeProcess` registration.
2. Add byte counters to pipe output queues and histories.
3. Add PTY registry wiring and PTY buffer counters.
4. Expose `inspect_terminals_json()` for runtime-only data.
5. Add system enumeration and terminal host classification.
6. Add zombie and orphan heuristics.
7. Add diff mode and buffer hardening.

## Expected Early Findings

The most likely first issues this feature will expose are:

- output retained forever in history vectors
- PTY buffer history retained forever
- runtime objects dropped while descendants remain alive
- reader threads surviving longer than expected after child exit
- shell wrappers or terminal hosts surviving after workload exit

## Decision Notes

### Why start in Rust

The leak questions are about ownership and lifecycle. Those boundaries are defined in Rust now for process creation and PTY handling. Python wrappers are the wrong place to establish the canonical registry.

### Why include system-wide inspection

The user problem is not only "what do our wrapper objects hold" but "what zombie terminals exist on the system." Runtime-only introspection will miss orphaned descendants and terminal hosts.

### Why not start with a public polished API

The near-term goal is diagnosis. A slightly raw but truthful snapshot API is more valuable than a polished but incomplete convenience layer.
