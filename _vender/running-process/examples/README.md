# running-process Rust examples

These examples are standalone Cargo packages. They compile against the local
`crates/running-process` public API and stay outside the main workspace so each
example declares the feature flags it needs.

Build all broker examples from the repository root:

```bash
uvx soldr cargo check --manifest-path examples/minimal-consumer/Cargo.toml
uvx soldr cargo check --manifest-path examples/release-handles-cli/Cargo.toml
uvx soldr cargo check --manifest-path examples/custom-isolation/Cargo.toml
```

## Examples

- [minimal-consumer](minimal-consumer/) builds a v1 `Hello` frame, prepares a
  `CacheManifest`, and shows the current `BackendHandle::probe_manifest`
  behavior before a daemon is recorded.
- [release-handles-cli](release-handles-cli/) wraps the stable
  `maintenance::run_release_handles` API as a small CLI.
- [custom-isolation](custom-isolation/) builds an `EXPLICIT_INSTANCE`
  service definition and derives the matching broker pipe for CI trust groups.

