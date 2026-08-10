# Integrate Your Daemon With the running-process Broker

The one-page recipe for consumers adopting the **full v1 broker path**
(zccache, soldr, clud, fbuild, or yours). The broker negotiates your
service version with a Hello handshake, points the client at a verified
backend endpoint, and hands back a ready-to-talk frame client.

This is the broker counterpart to [`INTEGRATE.md`](INTEGRATE.md). If you
only need the *minimal direct-endpoint* path (you already know the
daemon's socket and just want a verified frame lane), stay on
`INTEGRATE.md` — you do not need the broker. Everything else under
`docs/v1-*.md` is design rationale; the SDK API docs live on
`running_process::broker`.

What the broker adds over the direct path:

- **Version negotiation.** The client asks for a service + version range;
  the broker replies `Negotiated{ backend_pipe, daemon_version }` or a
  typed `Refused`. You never hard-code a socket path.
- **One-call adoption.** `BrokerSession::adopt` wraps the whole Hello →
  dial → frame-client recipe so consumer adoption is ≤15 lines.
- **Typed registration.** Builders produce + validate the
  `ServiceDefinition` (so the broker can find/spawn you) and the
  `CacheManifest` (so peers can discover your cache) without hand-written
  textproto.

Add the dependency. Tokio daemons should enable `client-async`:

```toml
[dependencies]
running-process = { version = "4", features = ["client-async"] }
```

## 1. Register a payload protocol

Same rule as the direct path: pick an unused ID from the
registered-consumer range `0x7000..=0x7EFF` (see
`running_process::broker::protocol::registry`), send a one-line PR adding
it to that table, and pin it in your crate:

```rust
running_process::register_payload_protocol! {
    /// my-daemon's opaque Frame lane.
    pub const MY_PAYLOAD_PROTOCOL: u32 = 0x7001;
}
```

Use `0xF000..=0xFFFF` (private use) for tests and closed deployments.

## 2. Register the service (`ServiceDefinition`)

The broker resolves a Hello to a backend by looking up a `ServiceDefinition`
you install once at provisioning time. Build it with
`ServiceDefinitionBuilder` instead of hand-writing textproto — it defaults
the boilerplate, validates on `build`, and writes the `.servicedef` for you:

```rust
use running_process::broker::builders::ServiceDefinitionBuilder;

// SHARED_BROKER: per-user local daemon (zccache, soldr, clud).
ServiceDefinitionBuilder::shared_broker("my-daemon", "/usr/local/bin/my-daemon")
    .min_version("1.10.0")
    .allow_version("1.11.20")
    .label("team", "infra")
    .install()?;                 // validate + write to the service-def dir

// EXPLICIT_INSTANCE: trust-grouped (CI pools).
// ServiceDefinitionBuilder::explicit_instance("my-daemon", abs_path, "ci-pool")
//     .allow_version("1.11.20")
//     .install()?;
```

`binary_path` must be absolute — a relative path is rejected on `build`.
Use `build()` to get the validated message without persisting, or
`install_in(dir)` to write into an explicit root (tests, custom layouts).

## 3. Publish the cache manifest (`CacheManifest`)

If peers discover your daemon's cache through the central registry,
publish a manifest with `CacheManifestBuilder`. `new` stamps the broker-owned
boilerplate (media type, schema version, host identity, timestamps);
`build` seals the `self_sha256` digest; `publish` writes it atomically:

```rust
use running_process::broker::builders::CacheManifestBuilder;
use running_process::broker::protocol::CacheRootKind;

CacheManifestBuilder::new("my-daemon", "1.11.20")
    .broker_instance("shared")
    .root(CacheRootKind::CacheData, "/var/cache/my-daemon")
    .publish()?;                 // seal + write to the central registry
```

Use `publish_in(dir)` for an explicit registry root in tests.

## 4. Connect through the broker

### Async (tokio daemons — default)

`AsyncBrokerSession::adopt` runs the blocking Hello negotiation on
`spawn_blocking`, then gives you an `.await`-able session. Inputs are owned
(`OwnedConnectRequest`) because they cross the worker-thread boundary:

```rust
use running_process::broker::adopt::{AsyncBrokerSession, OwnedConnectRequest};

let request = OwnedConnectRequest::new(
    broker_endpoint,                 // the broker's pipe/socket
    "my-daemon",                     // service name
    env!("CARGO_PKG_VERSION"),       // wanted version
    env!("CARGO_PKG_VERSION"),       // this client's own version
);
let mut session = AsyncBrokerSession::adopt(request).await?;

// session.route() == BackendConnectionRoute::BrokerNegotiated
// session.endpoint() is the negotiated backend, cacheable for Hello-skip
// session.negotiated().daemon_version is the version the broker chose
let response = session.request(MY_PAYLOAD_PROTOCOL, my_encoded_request).await?;
```

### Blocking (sync daemons or non-tokio runtimes)

`BrokerSession::adopt` is the wire-of-record; the async session is a thin
wrapper over it. It borrows `&str`, so no owned mirror is needed:

```rust
use running_process::broker::adopt::BrokerSession;
use running_process::broker::client::ConnectBackendRequest;

let request = ConnectBackendRequest::new(
    broker_endpoint, "my-daemon", env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_VERSION"),
);
let mut session = BrokerSession::adopt(request)?;
let response = session.request(MY_PAYLOAD_PROTOCOL, my_encoded_request)?;
```

Both honour the escape hatch first: with `RUNNING_PROCESS_DISABLE=1` set,
`adopt` returns `AdoptError::BrokerDisabled` so you fall back to your direct
(non-broker) path instead of silently dialing the broker.

## 5. Handle `Refused` with typed errors

A version mismatch or unknown service comes back as
`AdoptError::Connect(BrokerClientError::Refused { .. })`. Don't string-match
the reason — call `refusal_kind()` and branch on the `RefusalKind` enum:

```rust
use running_process::broker::adopt::AdoptError;
use running_process::broker::client::RefusalKind;

match AsyncBrokerSession::adopt(request).await {
    Ok(session) => { /* talk frames */ }
    Err(AdoptError::BrokerDisabled) => fallback_to_direct_path(),
    Err(AdoptError::Connect(err)) => match err.refusal_kind() {
        Some(RefusalKind::VersionUnsupported) => bail!("upgrade running-process"),
        Some(RefusalKind::VersionBlocked) => bail!("this daemon version is blocked"),
        Some(RefusalKind::ServiceUnknown) => bail!("install the .servicedef (step 2)"),
        Some(RefusalKind::RateLimited) => retry_with_backoff(),
        Some(RefusalKind::ShuttingDown) => retry_later(),
        Some(RefusalKind::Other(code)) => bail!("broker refused: {code:?}"),
        None => return Err(err.into()),   // not a refusal — a dial/IO error
    },
    Err(other) => return Err(other.into()),
}
```

`refusal_kind()` returns `None` for non-refusal failures (the broker was
unreachable, the backend dial failed), so a `Some(_)` always means the
broker spoke and declined.

## 6. Test it

- **End-to-end adoption:** model on
  `crates/running-process/tests/broker/toy_three_party.rs` — it wires all
  three parties (client, broker daemon, app daemon) in one process and
  exercises both `BrokerSession::adopt` and `AsyncBrokerSession::adopt`.
- **Refusal classification:** model on the `refusal_kind` tests in
  `crates/running-process/tests/broker/client.rs`.
- **Conformance kit:** assert v1 conformance with a single call from your
  adoption PR (see `crates/running-process/tests/broker/conformance_kit.rs`).
- **Escape hatch:** honour `RUNNING_PROCESS_DISABLE=1` by taking the direct
  path; `adopt` surfaces it as `AdoptError::BrokerDisabled`.

## Rules

- The broker API is **frozen** after #433 — `BrokerSession::adopt`, the
  builders, and `RefusalKind` will not churn, so you can pin to them.
- `RUNNING_PROCESS_FAKE_BACKEND` is a test-only seam behind the
  `test-seams` feature; it is never compiled into production consumers.
  `RUNNING_PROCESS_DISABLE=1` is the only production env knob.
- Never change a registered payload-protocol ID or your frozen golden bytes.
- Windows endpoint paths are bare pipe names (no `\\.\pipe\` prefix).
