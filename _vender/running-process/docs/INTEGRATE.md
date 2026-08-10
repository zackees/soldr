# Integrate Your Daemon With running-process

The one-page recipe for consumer CLIs (zccache, soldr, fbuild, clud, or
yours). This is the **only** document you need for the current minimal
regime; everything else under `docs/v1-*.md` is design rationale and
reference. SDK API docs live on
`running_process::broker::backend_sdk`.

What you get:

- **Verified daemon discovery.** Clients probe your daemon with
  `BackendHandle::probe_with_service` — nonce challenge over IPC plus
  pid/exe-hash/boot-id verification — instead of trusting a PID file.
- **An opaque frame lane.** Your requests/responses travel inside v1
  `Frame` envelopes on your existing endpoint, coexisting with your
  legacy wire byte-for-byte.

Add the dependency (the default `client` feature is all you need):

```toml
[dependencies]
running-process = "4"
```

## 1. Register a payload protocol

Pick an unused ID from the registered-consumer range `0x7000..=0x7EFF`
(see the table in `running_process::broker::protocol::registry`) and
send a one-line running-process PR adding it to that table. Pin it in
your crate:

```rust
running_process::register_payload_protocol! {
    /// my-daemon's opaque Frame lane.
    pub const MY_PAYLOAD_PROTOCOL: u32 = 0x7001;
}
```

Compile-time asserts reject first-party collisions and out-of-range
IDs. Use `0xF000..=0xFFFF` (private use) for tests and closed
deployments.

## 2. Daemon side: persist identity, serve the mux

At startup, build your identity and write the sidecar next to your PID
file (remove it on clean shutdown):

```rust
use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_sdk::write_daemon_identity_file;
use running_process::broker::protocol::Endpoint;

// Windows: BARE pipe name (no \\.\pipe\ prefix — the constructor
// rejects it). Unix: socket filesystem path.
let endpoint = Endpoint::unix_socket("my-daemon", "/run/my-daemon.sock")?;
let daemon = DaemonProcess::current_process(endpoint, Some(300))?;
write_daemon_identity_file(&identity_path, &daemon)?;
```

In the accept loop, route every accepted connection's buffered bytes
through `BackendEndpointMux`. It answers `BackendHandle` probes, hands
you decoded payload frames, and steps aside for your legacy wire:

```rust
use running_process::broker::backend_sdk::{
    BackendEndpointMux, LegacyClassification, MuxPoll,
};
use running_process::broker::protocol::{encode_framed, Frame};

let mux = BackendEndpointMux::new(daemon, &[MY_PAYLOAD_PROTOCOL], |buf| {
    // You know your legacy header; running-process does not. Return
    // Legacy / NotLegacy / NeedMoreBytes. No legacy wire? Always
    // return LegacyClassification::NotLegacy.
    my_legacy_header_check(buf)
});

// Per connection: `buf` is your growable read buffer.
loop {
    match mux.poll(&buf)? {
        MuxPoll::NeedMoreBytes => { /* read more bytes into buf */ }
        MuxPoll::Legacy => { /* your existing decoder owns buf now */ }
        MuxPoll::ProbeAnswered { reply, consumed } => {
            write_all(&reply)?;            // probe answered for you
            buf.drain(..consumed);
        }
        MuxPoll::Payload { frame, consumed } => {
            buf.drain(..consumed);
            let result = dispatch(&frame.payload)?;   // your handler
            let response = Frame::response_to(&frame, result);
            write_all(&encode_framed(&response)?)?;
        }
    }
}
```

The mux is sans-io (a pure function of the buffer), so it works under
tokio, async-std, threads, or blocking I/O unchanged. Connection-fatal
`MuxError`s mean drop the connection.

## 3. Client side: probe, then talk

### Async (tokio daemons — default)

Tokio daemons (zccache, soldr, clud) should enable the
`client-async` cargo feature and use the async surface so probes and
requests don't have to be wrapped in `spawn_blocking` at every call
site:

```toml
[dependencies]
running-process = { version = "4", features = ["client-async"] }
```

```rust
use running_process::broker::backend_handle::BackendHandle;
use running_process::broker::backend_sdk::{read_daemon_identity_file, AsyncFrameClient};

let Some(expected) = read_daemon_identity_file(&identity_path) else {
    return fallback_to_pid_file(); // old daemons, missing sidecar
};
let handle = BackendHandle::probe_with_service_async(
    "my-daemon", env!("CARGO_PKG_VERSION"), &expected.ipc_endpoint, &expected,
).await?;

let mut client = AsyncFrameClient::connect(&handle.daemon_process.ipc_endpoint).await?;
let response = client.request(MY_PAYLOAD_PROTOCOL, my_encoded_request).await?;
// response.payload is yours; request-id correlation already verified.
```

The async layer holds the canonical synchronous probe/frame wire and
runs each round-trip on `tokio::task::spawn_blocking`. The wire is
frozen v1 and identical to the blocking surface; the only thing that
differs at the call site is `.await` instead of an outer
`spawn_blocking` wrap.

### Blocking (sync daemons or non-tokio runtimes)

```rust
use running_process::broker::backend_handle::BackendHandle;
use running_process::broker::backend_sdk::{read_daemon_identity_file, FrameClient};

let Some(expected) = read_daemon_identity_file(&identity_path) else {
    return fallback_to_pid_file();
};
let handle = BackendHandle::probe_with_service(
    "my-daemon", env!("CARGO_PKG_VERSION"), &expected.ipc_endpoint, &expected,
)?;
let mut client = FrameClient::connect(&handle.daemon_process.ipc_endpoint)?;
let response = client.request(MY_PAYLOAD_PROTOCOL, my_encoded_request)?;
```

These blocking entry points are correct from synchronous code and
from `spawn_blocking` worker threads; calling them directly from an
async task without `spawn_blocking` blocks the runtime worker thread.

## 4. Test it

- **Golden bytes:** encode one request with
  `encode_framed(&Frame::request(MY_PAYLOAD_PROTOCOL, payload).with_request_id(n))`,
  assert the exact byte string, and freeze it (your wire must never
  drift). Model: `crates/running-process/tests/broker/golden_bytes.rs`.
- **Live mux round-trip + probe coexistence:** model on
  `crates/running-process/tests/broker/backend_sdk.rs` — that test's
  `serve_connection` is the canonical accept-loop shape.
- **Escape hatch:** honor `RUNNING_PROCESS_DISABLE=1` by skipping the
  probe and frame lane entirely (direct legacy path).

## Rules

- Never change a registered payload-protocol ID or your frozen golden
  bytes.
- Windows endpoint paths are bare pipe names; `Endpoint::windows_pipe`
  enforces this.
- `RUNNING_PROCESS_DISABLE=1` must always restore pre-adoption
  behavior.
