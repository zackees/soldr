# soldr Integration

Tracking issue: [#129](https://github.com/zackees/soldr/issues/129)

This file is for an AI or human wiring `soldr` into a project build.

## Rules

1. The integration point is `soldr cargo ...`.
2. Do not manually set `RUSTC_WRAPPER` for the normal path. `soldr cargo ...` does that for you.
3. After `soldr` is installed, the build change is usually one line:

```diff
- cargo build --locked --release
+ soldr cargo build --locked --release
```

The same pattern applies to `cargo test`, `cargo check`, and similar Cargo invocations.

## GitHub Actions

No public one-line `uses: owner/repo@ref` Soldr action is verified in this repository today. The supported path is:

1. install `soldr`
2. replace `cargo ...` with `soldr cargo ...`

### Shortest CI path: install a released soldr

Pin the current release line when you want the shortest workflow.

```yaml
name: ci

on:
  push:
  pull_request:

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5 # v4

      - uses: dtolnay/rust-toolchain@aad518f59d88bae90133242f9ddac7f8bbc5dddf # 1.94.1

      - name: Install soldr 0.7.4
        shell: bash
        run: |
          curl -fsSL https://raw.githubusercontent.com/zackees/soldr/v0.7.4/install.sh | bash -s -- --version 0.7.4
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"

      - name: Build through soldr
        shell: bash
        run: soldr cargo build --locked --release

      - name: Test through soldr
        shell: bash
        run: soldr cargo test --locked
```

Use this when Linux CI is enough and you want the shortest setup.

### Always-works CI path: build soldr from source and use the local binary

This is the fallback that works for local development and for GitHub Actions builds because it does not depend on a published setup action or a preinstalled `soldr`.

```yaml
name: ci

on:
  push:
  pull_request:

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5 # v4

      - uses: actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5 # v4
        with:
          repository: zackees/soldr
          ref: v0.7.4
          path: soldr

      - uses: dtolnay/rust-toolchain@aad518f59d88bae90133242f9ddac7f8bbc5dddf # 1.94.1

      - name: Build soldr from source
        working-directory: soldr
        shell: pwsh
        run: cargo build --package soldr-cli --release --locked

      - name: Build project through local soldr
        shell: pwsh
        run: |
          $ext = if ($env:RUNNER_OS -eq "Windows") { ".exe" } else { "" }
          $soldr = Join-Path $env:GITHUB_WORKSPACE "soldr/target/release/soldr$ext"
          & $soldr cargo build --locked --release

      - name: Test project through local soldr
        shell: pwsh
        run: |
          $ext = if ($env:RUNNER_OS -eq "Windows") { ".exe" } else { "" }
          $soldr = Join-Path $env:GITHUB_WORKSPACE "soldr/target/release/soldr$ext"
          & $soldr cargo test --locked
```

If your workflow already installs Rust and already has a build step, the real behavioral change is still just prefixing Cargo with `soldr`.

## Local Builds

### Installed soldr

If `soldr` is already on `PATH`, use:

```bash
soldr cargo build --locked --release
soldr cargo test --locked
```

### Local source build of soldr

This is the safest fallback because it uses the local checkout directly.

Build soldr:

```bash
cargo build --package soldr-cli --release --locked
```

Then run your build through the locally built binary.

On macOS/Linux:

```bash
./target/release/soldr cargo build --locked --release
./target/release/soldr cargo test --locked
```

On Windows PowerShell:

```powershell
.\target\release\soldr.exe cargo build --locked --release
.\target\release\soldr.exe cargo test --locked
```

## AI Checklist

When updating a workflow for `soldr`, do this:

1. Keep the existing Rust toolchain setup.
2. Install `soldr` or build `soldr` from source.
3. Replace each `cargo ...` build/test/check command with `soldr cargo ...`.
4. Do not add manual `RUSTC_WRAPPER` wiring unless the workflow explicitly needs wrapper-mode testing.
5. Use the local source-build path when you need the most reliable cross-environment fallback.
