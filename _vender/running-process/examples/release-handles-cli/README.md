# release-handles-cli

This example wraps the stable `maintenance::run_release_handles` API as a
small command-line program. It accepts one path argument and prints the stable
JSON shape returned by the Rust API.

Build:

```bash
uvx soldr cargo check --manifest-path examples/release-handles-cli/Cargo.toml
```

Run:

```bash
uvx soldr cargo run --manifest-path examples/release-handles-cli/Cargo.toml -- .
```

