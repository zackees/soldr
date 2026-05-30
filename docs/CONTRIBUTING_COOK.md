# Contributing to the shared `soldr cook` artifact cache

> [!CAUTION]
> ## Build and test in Docker only
>
> **Never run `cargo test` or `cargo build` on the host while working on issues
> #576, #577, or #578 (meta #579).** These PRs mutate:
>
> - The soldr daemon protocol (`crates/soldr-cli/src/daemon/protocol.rs`)
> - The shared redb schema at `~/.soldr/state.redb` (new `cook_index_v1` table)
> - The cargo-front-door pre-flight hot path (PR 3)
>
> `soldr` installs as a per-user singleton. Host-side `cargo test`
> for this feature would corrupt the host developer's running daemon,
> persistent redb state, and (in PR 2/3) the `~/.soldr/cache/cook/`
> artifact tree. The Docker harness in `docker/cook-shared-cache/`
> mounts `~/.soldr/` as a container-local named volume so the host
> singleton stays untouched.

---

## The supported loop

```bash
# Build + run the dormant cook-index integration tests (PR 1 / #576).
bench/cook_in_docker.sh

# Run the full workspace test suite in Docker.
bench/cook_in_docker.sh cargo test --workspace

# Run clippy.
bench/cook_in_docker.sh cargo clippy --workspace -- -D warnings

# Run a single integration test by name.
bench/cook_in_docker.sh cargo test --workspace --test daemon_cook_index \
    -- --include-ignored cook_record_then_lookup_round_trips
```

`bench/cook_in_docker.sh` builds the
`docker/cook-shared-cache/Dockerfile` image (cached after the first
run), then runs your command inside a container with:

- The source tree mounted read-write at `/work`.
- `~/.soldr/` provided by a fresh named volume `cook-soldr-home` at
  `/root/.soldr` inside the container. The host's actual `~/.soldr/`
  is NEVER bind-mounted.
- `SOLDR_COOK_DOCKER_HARNESS=1` exported so the tests gated on the
  Docker marker run.

The script `docker volume rm`s `cook-soldr-home` at the start of each
run so the container always starts from an empty soldr state. Comment
out that line locally if you want to debug across runs.

## Acceptance gate (per PR in meta #579)

Each PR's CI workflow MUST exercise the Docker harness AND assert
that the host's `~/.soldr/` is byte-identical before and after the
suite. The harness mount semantics make this trivial — the host path
is never touched — but the assertion catches accidental bind-mount
regressions in future workflow edits.

## The container marker

Every integration test in `tests/daemon_cook_index.rs` (and the
equivalent tests in PRs 2 and 3) starts with:

```rust
if skip_unless_in_container("test_name") { return; }
```

This short-circuits the test with a `println!` when
`SOLDR_COOK_DOCKER_HARNESS` is not set. Result: bare-host `cargo
test` runs are no-ops for these tests, and only `bench/cook_in_docker.sh`
(which exports the marker) actually exercises them.

## What lives where

- `docker/cook-shared-cache/Dockerfile` — Rust 1.94.1 base image with
  `pkg-config`, `libssl-dev`, `git`. Marker env var `SOLDR_COOK_DOCKER_HARNESS=1`.
- `bench/cook_in_docker.sh` — supported runner. Builds the image,
  mounts the source tree, runs the requested command.
- `docs/CONTRIBUTING_COOK.md` — this file.

## PR scope reminder

- PR 1 (#576) — dormant `cook_index_v1` redb table + new IPC variants
  + extended `Status` reply. No `soldr cook` changes. No
  cargo-front-door changes.
- PR 2 (#577) — `soldr cook` writes `<sha256>.tar.zst` artifacts +
  registers them via `CookRecord` + emits Cargo.lock-tracked /
  no-`.git/` / gitignored warnings.
- PR 3 (#578) — cargo-front-door pre-flight auto-hydrate (default ON,
  green status line) + opt-out via env var > `rust-toolchain.toml` >
  `~/.soldr/config.toml`.

See the meta issue for the full design lock-in.
