# custom-isolation

This example builds the `EXPLICIT_INSTANCE` configuration used for CI trust
grouping. It derives the platform broker pipe for a named instance, prepares a
`ServiceDefinition`, and mirrors the instance name into a `CacheManifest`.

Build:

```bash
uvx soldr cargo check --manifest-path examples/custom-isolation/Cargo.toml
```

Run with the default `ci-trusted` instance:

```bash
uvx soldr cargo run --manifest-path examples/custom-isolation/Cargo.toml
```

Run with a separate lowercase instance:

```bash
RUNNING_PROCESS_EXPLICIT_INSTANCE=ci-untrusted uvx soldr cargo run --manifest-path examples/custom-isolation/Cargo.toml
```

