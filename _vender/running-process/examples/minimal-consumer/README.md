# minimal-consumer

This example is a tiny Rust consumer for the v1 broker surface that exists
today. It builds a `Hello` protobuf message, wraps it in the frozen v1 frame
layout, prepares a self-hashed `CacheManifest`, and probes that manifest with
`BackendHandle`.

Build:

```bash
uvx soldr cargo check --manifest-path examples/minimal-consumer/Cargo.toml
```

Run:

```bash
uvx soldr cargo run --manifest-path examples/minimal-consumer/Cargo.toml
```

