# Responsive Downloader Plan

## Summary

Add a native Rust downloader that is exposed through the Python bindings as a small API object and coordinated by a separate per-user download daemon.

The key decision is:

- Use a second per-user daemon process dedicated to downloads.
- Inside that daemon, create one logical download worker per canonical destination path.
- Do not spawn one OS daemon process per path.

That isolates downloads from the fast-changing main `zccache` daemon while still avoiding process explosion from one process per destination.

## Requirements Mapped To Design

- Fast like `aria2c`: use concurrent HTTP range requests when the server supports ranges.
- Cross-platform, internal libs only: implement in Rust with `tokio` + `reqwest`/`hyper` + `rustls`; no shelling out.
- Keep as much logic in Rust as possible:
  - Python should be a thin wrapper over native objects
  - download orchestration, path identity, IPC, progress tracking, retries, cancellation, and cleanup should all live in Rust
- Python API inputs: `source_url` and `destination_path`.
- Multiple concurrent clients:
  - first attacher becomes initiator
  - later attachers join the same in-flight job
  - everyone sees the same aggregate progress
- Path identity:
  - key jobs by canonical destination path
  - hash that canonical key to form a stable job id
- Cancellation:
  - a single client cancel only detaches that client
  - if the last client detaches/cancels, abort the download, delete the temp file, log the event, and destroy the job
- Python/Rust bindings:
  - add a native-backed downloader object
  - export it from the top-level Python package

## Main Decision: Logical Worker, Not Per-Path Process

The main `zccache` daemon is under heavy development and gets restarted often. That makes it the wrong place to host long-lived downloads, because normal development churn would disrupt unrelated tools.

So the right split is:

- keep the existing main daemon focused on compile/cache work
- add a second per-user daemon dedicated to downloads
- inside the download daemon, keep one logical worker per canonical destination path

If we created a separate OS daemon per destination path, we would need:

- endpoint naming/discovery per path
- lock files per path
- process supervision
- stale process cleanup
- duplicate logging/runtime/bootstrap code

None of that improves download correctness or speed. The useful part of your idea is the per-path uniqueness. We should keep that as an in-daemon worker identity:

- `download_key = hash(canonical_destination_path)`
- `jobs: DashMap<DownloadKey, Arc<DownloadJob>>`

That gives:

- operational isolation from the main daemon
- a stable daemon that rarely changes
- "unique daemon per path" behavior as a logical worker model, not a process model

## Separate Download Daemon

Add a dedicated binary/crate pair:

- `crates/zccache-download-daemon`
- optionally `crates/zccache-download-client` if the client code needs to stay separate from the existing CLI crate

Add a dedicated user-facing CLI:

- `zccache-download`

The CLI should talk to the download daemon through the Rust client library, not implement downloader logic itself.

Responsibilities of the download daemon:

- own the download job table
- accept Python client attachments over IPC
- run segmented HTTP downloads
- handle last-client-detach cleanup
- keep its own logs, lock file, and temp metadata

This daemon should have:

- its own IPC endpoint
- its own lock file
- its own versioning boundary
- its own startup helper from the Python binding layer

Recommended endpoint naming:

- Unix: `$XDG_RUNTIME_DIR/zccache-download/sock` or `/tmp/zccache-download-{uid}/sock`
- Windows: `\\\\.\\pipe\\zccache-download-{username}`

Recommended lock file:

- `~/.zccache/download-daemon.lock`

Recommended logs:

- `~/.zccache/logs/download-daemon.log`

Recommended state root:

- `~/.zccache/downloads/`

## Canonical Path Identity

Use the destination path, not the URL, as the dedupe key.

Algorithm:

1. Convert the requested destination to an absolute path.
2. Canonicalize the parent directory with `std::fs::canonicalize`.
3. Join the filename back onto the canonical parent.
4. Normalize the result with `NormalizedPath`.
5. Hash `normalize_for_key(canonical_destination)` with `blake3` to get `download_key`.

Notes:

- This works even when the destination file does not exist yet.
- It resolves symlinked parent directories, which matters for correct dedupe.
- If a second client attaches with the same destination but a different URL while a job is active, return an error instead of merging them.

## Download Engine

Create a new crate:

- `crates/zccache-download`

Responsibilities:

- probe remote support with `HEAD` or `GET bytes=0-0`
- detect `Content-Length`, `Accept-Ranges`, `ETag`, `Last-Modified`
- choose single-stream or segmented mode
- run concurrent range requests
- aggregate progress
- retry failed segments with bounded backoff
- write to a temp file and atomically rename on success

Recommended stack:

- `reqwest` with `rustls-tls`
- `tokio`
- `bytes`
- `futures`

Important HTTP details:

- send `Accept-Encoding: identity` so ranged responses are not transparently decompressed
- only enable multi-socket mode when:
  - `Content-Length` is known
  - the server supports byte ranges
  - the file is above a minimum size threshold
- otherwise fall back to a single request

Suggested segment policy:

- default min segment size: `8 MiB`
- default max parallel segments: `min(available_parallelism * 2, 16)`
- small files stay single-stream

## Layering Rule: Rust Owns The Logic

The intended layering is:

- `zccache-download`: core downloader engine and download-domain types
- `zccache-download-protocol`: IPC messages and wire types
- `zccache-download-daemon`: job manager, worker lifecycle, and daemon process
- `zccache-download-client`: persistent client connection API used by CLI and Python bindings
- `zccache-download` CLI binary: user-facing terminal commands only
- Python bindings: thin adapters over the Rust client API

Python should not contain:

- download scheduling
- progress math
- retry policy
- canonical path identity logic
- temp file naming
- daemon discovery/startup logic
- cancellation semantics
- polling loops beyond direct calls into Rust

Python should contain only:

- ergonomic dataclasses and wrappers
- argument coercion (`str | Path` to string)
- context-manager convenience

## zccache-download CLI

Add a dedicated binary:

- `crates/zccache-download-cli`
- installed command name: `zccache-download`

Purpose:

- manual use
- debugging the daemon independently of Python
- stable smoke tests for the download subsystem

Recommended commands:

- `zccache-download get <url> <destination>`
- `zccache-download wait <url> <destination>`
- `zccache-download status <url> <destination>`
- `zccache-download cancel <url> <destination>`
- `zccache-download daemon start`
- `zccache-download daemon stop`
- `zccache-download daemon status`

Recommended command behavior:

- `get`:
  - ensure the download daemon is running
  - attach to the destination-scoped job
  - print whether this invocation is the initiator
  - stream progress to stdout until completion unless `--detach` is requested
- `wait`:
  - attach to an existing job and block until terminal state
- `status`:
  - return a single snapshot for scripts
- `cancel`:
  - cancel this client attachment
  - if this is the last client, the daemon aborts and cleans up
- `daemon start/stop/status`:
  - mirror the lifecycle controls that already exist for the main daemon, but scoped to the download daemon

Recommended flags:

- `--json`
- `--detach`
- `--timeout-ms <n>`
- `--endpoint <custom-endpoint>`
- `--connections <n>`
- `--min-segment-size <bytes>`
- `--force`

Important rule:

- the CLI is a thin shell over the Rust client library
- it must not reimplement progress calculations, retries, or job coordination

## File Layout

Use a sibling temp file so finalization is an atomic rename on the same filesystem.

- final path: user destination
- temp path: `.<filename>.zccache-download-<short_hash>.part`
- daemon metadata path: `~/.zccache/tmp/downloads/<download_key>/`

Keep temp artifacts separate:

- temp payload sits next to destination for atomic rename
- daemon state lives under the existing cache dir

If the last client leaves before completion:

- abort all segment tasks
- close file handles
- delete sibling temp file
- remove `~/.zccache/tmp/downloads/<download_key>/`
- log `abandoned`

## In-Daemon Data Model

Add a download manager to the dedicated download daemon shared state.

```rust
struct DownloadManager {
    jobs: DashMap<DownloadKey, Arc<DownloadJob>>,
}

struct DownloadJob {
    key: String,
    url: String,
    destination: NormalizedPath,
    temp_path: NormalizedPath,
    state: tokio::sync::RwLock<DownloadSnapshot>,
    clients: DashMap<ClientId, ClientLease>,
    updates: tokio::sync::watch::Sender<DownloadSnapshot>,
    cancel_token: tokio_util::sync::CancellationToken,
    worker: tokio::task::JoinHandle<()>,
}

struct DownloadSnapshot {
    phase: DownloadPhase,
    total_bytes: Option<u64>,
    downloaded_bytes: u64,
    percentage: Option<f32>,
    active_clients: u32,
    segments_total: u16,
    segments_active: u16,
    error: Option<String>,
}
```

`percentage` should be computed from raw counters and rounded only when sent to the client:

- `Some(round2(downloaded * 100.0 / total))` when total is known
- `None` when total is unknown

I do not recommend forcing `0.00` when the total size is unknown. That is less truthful than `None`.

## Client Attachment Model

Each Python download handle should own one long-lived IPC connection.

That connection is the client lease.

Behavior:

- `download()` attaches to the job and returns:
  - whether this client is the initiator
  - the current snapshot
- dropping the handle or losing the IPC connection detaches that client
- `cancel()` detaches that client explicitly
- when the client count reaches zero, the daemon aborts and cleans up

This is simpler and more reliable than heartbeat-based ephemeral requests.

## Protocol Additions

Define a small download-specific protocol for the dedicated download daemon. This can live either:

- in `zccache-protocol` if we want one shared protocol crate, or
- in a new `zccache-download-protocol` crate if we want stricter isolation from compile-daemon churn

Given your stability goal, I recommend a separate download protocol crate so changes in the main daemon protocol do not force download-daemon rebuilds or compatibility churn.

Suggested request variants:

- `DownloadAttach { url, destination }`
- `DownloadStatus`
- `DownloadWait { timeout_ms: Option<u64> }`
- `DownloadCancel`

Suggested response variants:

- `DownloadAttached { download_id, initiator, status }`
- `DownloadStatusResult { status }`
- `DownloadFinished { status }`
- `DownloadCancelled { status }`

Because the handle owns a dedicated connection, `DownloadStatus`, `DownloadWait`, and `DownloadCancel` can be connection-scoped and do not need to repeat ids on every call.

`DownloadWait` should block until:

- status changes
- timeout expires
- terminal state is reached

That avoids busy polling from Python while still keeping the protocol simple.

If CLI support wants non-attachment queries later, add explicit keyed requests such as:

- `DownloadLookup { destination }`
- `DownloadLookupStatus { download_id }`

but v1 should prefer the attach-based model because it aligns with the client-lease semantics.

## Rust API

Expose a stable Rust client API that both the CLI and Python bindings use.

Primary crates:

- `crates/zccache-download`
- `crates/zccache-download-client`

Suggested Rust API surface:

```rust
pub struct DownloadOptions {
    pub force: bool,
    pub max_connections: Option<usize>,
    pub min_segment_size: Option<u64>,
}

pub struct DownloadStatus {
    pub phase: DownloadPhase,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub percentage: Option<f32>,
    pub active_clients: u32,
    pub destination: NormalizedPath,
    pub source_url: String,
    pub error: Option<String>,
}

pub struct DownloadAttachResult {
    pub download_id: String,
    pub initiator: bool,
    pub status: DownloadStatus,
}

pub struct DownloadClient {
    // endpoint config
}

pub struct DownloadHandle {
    // owns one persistent IPC connection
}

impl DownloadClient {
    pub fn new(endpoint: Option<String>) -> Self;
    pub fn start_daemon(&self) -> Result<(), String>;
    pub fn stop_daemon(&self) -> Result<bool, String>;
    pub fn daemon_status(&self) -> Result<DownloadDaemonStatus, String>;
    pub fn download(
        &self,
        url: &str,
        destination: &Path,
        options: DownloadOptions,
    ) -> Result<DownloadHandle, String>;
}

impl DownloadHandle {
    pub fn initiator(&self) -> bool;
    pub fn download_id(&self) -> &str;
    pub fn status(&mut self) -> Result<DownloadStatus, String>;
    pub fn wait(&mut self, timeout_ms: Option<u64>) -> Result<DownloadStatus, String>;
    pub fn cancel(&mut self) -> Result<DownloadStatus, String>;
    pub fn close(&mut self) -> Result<(), String>;
}
```

Design rules:

- all public progress/status types should be Rust-defined first
- CLI formatting and Python dataclasses should map from these Rust types
- any rounding or derived-field logic should happen in Rust before crossing FFI

Recommended additional Rust types:

- `DownloadPhase`
- `DownloadDaemonStatus`
- `DownloadError`

`DownloadError` should carry structured categories internally even if v1 stringifies them across the Python boundary.

## Python API

Add a new Python module:

- `python/zccache/downloader.py`

Add new public types:

- `DownloadApi`
- `DownloadHandle`
- `DownloadStatus`

Python should be intentionally thin. The Python layer should call the native Rust handle methods directly and avoid implementing downloader behavior itself.

Proposed surface:

```python
from pathlib import Path
from zccache import DownloadApi

api = DownloadApi()
handle = api.download(
    source_url="https://example.com/toolchain.tar.zst",
    destination=Path("cache/toolchain.tar.zst"),
)

handle.initiator
status = handle.status()
status.total_bytes
status.downloaded_bytes
status.percentage

final = handle.wait()
handle.cancel()
handle.close()
```

Suggested API:

```python
class DownloadApi:
    def __init__(self, endpoint: str | None = None) -> None: ...
    def start(self) -> None: ...
    def stop(self) -> bool: ...
    def daemon_status(self) -> DownloadDaemonStatus: ...
    def download(
        self,
        *,
        source_url: str,
        destination: str | Path,
        force: bool = False,
        max_connections: int | None = None,
        min_segment_size: int | None = None,
    ) -> DownloadHandle: ...

class DownloadHandle:
    @property
    def initiator(self) -> bool: ...
    @property
    def download_id(self) -> str: ...
    def status(self) -> DownloadStatus: ...
    def wait(self, timeout_ms: int | None = None) -> DownloadStatus: ...
    def cancel(self) -> DownloadStatus: ...
    def close(self) -> None: ...
    def __enter__(self) -> DownloadHandle: ...
    def __exit__(self, exc_type, exc, tb) -> None: ...
```

Suggested dataclasses:

```python
@dataclass(frozen=True)
class DownloadStatus:
    phase: str
    total_bytes: int | None
    downloaded_bytes: int
    percentage: float | None
    active_clients: int
    initiator: bool
    destination: str
    source_url: str
    error: str | None = None
```

```python
@dataclass(frozen=True)
class DownloadDaemonStatus:
    version: str
    active_downloads: int
    connected_clients: int
    uptime_secs: int
    endpoint: str
```

Notes:

- `initiator` is fixed per handle and should also be copied into the Python status object for convenience.
- `wait()` should return the terminal `DownloadStatus`.
- `close()` should detach without raising if already terminal.
- the Python dataclasses should be populated from native Rust result objects, not computed independently in Python

## Rust Binding Changes

Add new native classes in the Python binding layer:

- `NativeDownloadApi`
- `NativeDownloadHandle`
- `NativeDownloadStatus`

Add matching client-side support in a download client library used by the bindings:

- persistent connection wrapper for download handles
- attach/status/wait/cancel methods

Then add the Python wrapper in:

- `python/zccache/downloader.py`
- `python/zccache/__init__.py`

Recommended native class layout:

- `NativeDownloadApi`
  - owns endpoint configuration
  - exposes `start`, `stop`, `daemon_status`, `download`
- `NativeDownloadHandle`
  - wraps the Rust `DownloadHandle`
  - exposes `initiator`, `download_id`, `status`, `wait`, `cancel`, `close`
- `NativeDownloadStatus`
- `NativeDownloadDaemonStatus`

Important rule:

- `python/zccache/downloader.py` should be mostly coercion and dataclass conversion
- daemon startup, endpoint resolution, IPC retries, and handle lifecycle should stay in Rust

## Logging

Use a separate logger for the dedicated download daemon.

Log download lifecycle events to:

- `~/.zccache/logs/download-daemon.log`

Events to log:

- attach
- join
- download start
- mode selection (`single` vs `segmented`)
- completion
- client detach
- abandoned because last client left
- failure with error summary

If detailed per-segment logs become noisy, add a dedicated `download.log` later. Start with the existing logger.

## Failure and Edge Cases

- Range not supported: fall back to single-stream mode.
- Unknown content length: stream single connection, `total_bytes=None`, `percentage=None`.
- Destination already exists:
  - recommended v1 behavior: treat as success and skip network work
  - if overwrite is required later, add `force=True`
- Different URL for same active destination: error.
- Download-daemon restart mid-download:
  - clients get a broken connection
  - temp file is cleaned on next attach or daemon startup scan
- Segment failure:
  - retry the segment only
  - if retries exhausted, fail the whole job and surface one shared terminal error to all clients

## Repo Changes

Expected files/modules:

- `crates/zccache-download/Cargo.toml`
- `crates/zccache-download/src/lib.rs`
- `crates/zccache-download-cli/Cargo.toml`
- `crates/zccache-download-cli/src/main.rs`
- `crates/zccache-download-daemon/Cargo.toml`
- `crates/zccache-download-daemon/src/main.rs`
- `crates/zccache-download-daemon/src/lib.rs`
- `crates/zccache-download-protocol/Cargo.toml`
- `crates/zccache-download-protocol/src/lib.rs`
- `crates/zccache-download-client/Cargo.toml`
- `crates/zccache-download-client/src/lib.rs`
- Python binding integration point for download native classes
- `python/zccache/downloader.py`
- `python/zccache/__init__.py`
- `python/tests/test_public_api.py`

## Test Plan

Rust unit tests:

- canonical destination key generation
- percentage rounding
- URL mismatch rejection for same active destination
- last-client-cancels cleanup
- single-stream fallback when ranges unsupported

Rust integration tests with a local HTTP server:

- segmented download completes and file matches
- multiple clients attach to one in-flight job
- second client is not initiator
- one client cancels while another remains attached
- all clients cancel and temp file is removed
- unknown `Content-Length` path

Python tests:

- top-level import exposes downloader symbols
- download handle returns initiator flag and progress fields
- wait/cancel/close behavior maps correctly to the native layer
- Python wrapper does not contain duplicated progress math or retry logic

CLI tests:

- `zccache-download get` starts or connects to the daemon and completes a download
- `zccache-download status` returns a snapshot
- `zccache-download cancel` detaches and triggers cleanup when last client leaves
- `zccache-download daemon status` reports daemon health

## Implementation Phases

### Phase 1: Protocol and API skeleton

- add protocol structs and variants
- add Rust client API types
- add `zccache-download` CLI skeleton
- add client-side persistent download handle
- add Python native and pure-Python wrappers
- add top-level exports

### Phase 2: Core single-stream downloader

- implement attach/join semantics
- implement single-stream HTTP download
- temp file + atomic rename
- cleanup on last-client-detach

### Phase 3: Segmented fast path

- add range probing
- add concurrent segment workers
- aggregate progress and retries

### Phase 4: Tests and hardening

- local HTTP test server coverage
- restart/cleanup tests
- log validation
- path canonicalization edge cases on Windows

## Recommendation

Build this as a separate per-user download daemon, with one path-scoped in-memory worker per canonical destination inside that daemon, a persistent IPC-backed Python handle per client, and segmented HTTP downloads only when the server proves it can support them.

That is the lowest-risk design that still satisfies the speed, cross-platform, binding, and multi-client coordination requirements while avoiding disruption from normal main-daemon restarts during development.
