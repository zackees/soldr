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

This repository publishes a public setup action for Soldr. The current GitHub Actions path is:

1. use `zackees/setup-soldr@v0`
2. let the action bootstrap `rustup` if needed, then provision the Rust toolchain and restore the Soldr/Cargo/rustup cache root
3. run `soldr cargo ...`

### Public-action status

The public beta UX is:

```yaml
steps:
  - uses: actions/checkout@v4

  - uses: zackees/setup-soldr@v0
    with:
      cache: true

  - run: soldr cargo build --locked --release
  - run: soldr cargo test --locked
```

The public action repository is [`zackees/setup-soldr`](https://github.com/zackees/setup-soldr). This repository remains the source of truth for the action implementation and exports the standalone action bundle from the root `action.yml` plus helper scripts. The extraction plan and `@v0` beta contract live in [docs/SETUP_SOLDR_PUBLIC_ACTION.md](./docs/SETUP_SOLDR_PUBLIC_ACTION.md).

Beta and stable tag rule for the public repo:

- `@v0` is the moving beta tag while the action contract is still settling
- `@v1` should be introduced only when the action is ready for a stable backward-compatible contract
- later major tags such as `@v2` are introduced only for breaking contract changes after `@v1`
- the intended normal path is still one `setup-soldr` step plus `soldr cargo ...`; no separate toolchain action is part of the common-case contract

### Preferred setup action path

The root action:

- installs one `soldr` binary
- preinstalls the exact Rust toolchain resolved from `rust-toolchain.toml` or `toolchain:` via `rustup`
- sets `SOLDR_CACHE_DIR`, `CARGO_HOME`, and `RUSTUP_HOME`
- restores and saves that runner-local root through GitHub cache when `cache: true`
- restores and saves the Soldr-owned zccache compilation artifact cache under `SOLDR_CACHE_DIR` by default; set `build-cache: false` to disable that layer

Important toolchain rule:

- if your repository already pins Rust in `rust-toolchain.toml`, let the action read that file or pass the exact channel with `toolchain:`
- do not preinstall a different generic toolchain such as `stable` and assume a later `soldr cargo ...` step will reconcile it
- the action exports `RUSTUP_TOOLCHAIN` after installation so later `cargo` and `rustc` calls keep using the preinstalled toolchain instead of asking `rustup` to resolve it on demand
- on GitHub-hosted runners, no separate toolchain setup action is usually needed for this path; the action will bootstrap `rustup` into its cached root if the runner does not already have it

Example:

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

      - uses: zackees/setup-soldr@v0
        with:
          cache: true

      - run: soldr cargo build --locked --release
      - run: soldr cargo test --locked
```

For local same-repository action development before exporting a new public release, use:

```yaml
- uses: ./
  with:
    cache: true
```

Useful inputs when wiring the action into another repository:

- `toolchain`: explicit Rust channel override when you do not want to rely on `rust-toolchain.toml`
- `toolchain-file`: alternate toolchain file path when the repo does not use the default root `rust-toolchain.toml`
- `cache`: turn the runner-local cache root on or off
- `build-cache`: turn the Soldr-owned zccache compilation artifact cache on or off; defaults to `true`
- `cache-dir`: move the shared Soldr/Cargo/rustup root to a specific path
- `trust-mode`: set `SOLDR_TRUST_MODE` for stricter fetched-binary policy

The current root `repo` input is an implementation/testing override for the in-repo action source. It is not part of the public `setup-soldr@v0` beta contract.

Useful outputs:

- `soldr-path`
- `soldr-version`
- `cache-dir`
- `cache-hit`
- `build-cache-hit`
- `toolchain`

### What gets rehydrated today

The action-managed cache root includes:

- `SOLDR_CACHE_DIR`
- `CARGO_HOME`
- `RUSTUP_HOME`

That means the Soldr binary, the Rust toolchain, cargo registry state, and Soldr-managed state are reusable on later runs.

The managed `zccache` artifact store is controlled by Soldr through `ZCCACHE_CACHE_DIR` and lives under `SOLDR_CACHE_DIR` by default. Use `cache-dir` to move the action-managed root for a workflow.

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
        with:
          toolchain: 1.94.1

      - name: Install latest soldr release
        shell: bash
        run: |
          curl -fsSL https://raw.githubusercontent.com/zackees/soldr/main/install.sh | bash
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
          ref: main
          path: soldr

      - uses: dtolnay/rust-toolchain@aad518f59d88bae90133242f9ddac7f8bbc5dddf # 1.94.1
        with:
          toolchain: 1.94.1

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

1. If you use the current root action on GitHub-hosted runners, do not add a separate toolchain setup action just for the normal path.
2. If you are not using the root action, keep or add explicit Rust toolchain setup yourself.
3. Install `soldr` or build `soldr` from source.
4. Replace each `cargo ...` build/test/check command with `soldr cargo ...`.
5. Do not add manual `RUSTC_WRAPPER` wiring unless the workflow explicitly needs wrapper-mode testing.
6. Use the local source-build path when you need the most reliable cross-environment fallback.
