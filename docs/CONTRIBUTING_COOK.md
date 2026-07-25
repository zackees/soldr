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
- `~/.soldr/` provided by a fresh named volume `cook-soldr-home-<root>`
  at `/root/.soldr` inside the container. The host's actual `~/.soldr/`
  is NEVER bind-mounted.
- `/work/target` provided by the **persistent** named volume
  `soldr-perf-target-<root>` (issue #593). Cargo's build state lives on
  Linux-native ext4 inside Docker's VFS, not on the host bind mount,
  so cargo's mtime-based fingerprint check actually succeeds across
  container restarts.
- `/root/.cargo` (`CARGO_HOME`) provided by the persistent named
  volume `soldr-perf-cargo-home-<root>`. Registry index + downloaded
  crates stay warm.
- `SOLDR_COOK_DOCKER_HARNESS=1` exported so the tests gated on the
  Docker marker run.

`<root>` is a `<leaf-name>-<hash>` suffix identifying the checkout, so
sibling checkouts never share these volumes — see "Per-checkout
isolation" below. Print the resolved names with:

```bash
SOLDR_COOK_PRINT_PLAN=1 bench/cook_in_docker.sh
```

The script `docker volume rm`s its own `cook-soldr-home-<root>` at the
start of each run so the container always starts from an empty soldr
state. The **warm** `soldr-perf-target-<root>` and
`soldr-perf-cargo-home-<root>` volumes are NEVER wiped here — that's
the entire point of issue #593's design.

## Why named volumes for `target/` and `CARGO_HOME`

Issue #593 fixes a Windows + Docker Desktop performance regression:
the WSL2 9P translation layer rewrites file mtimes per container
start. Cargo's mtime-based fingerprint check then decides every
crate is stale and rebuilds the entire workspace.

Measured on zccache's 21-crate workspace before the fix:

| Scenario                                  | Bind mount | Named volume |
|---|---|---|
| `cargo build --release --bin X` (cold)    | ~6 min     | ~4 m 22 s    |
| Same command, immediate rerun (no-op)     | **6 m 22 s** | **1.09 s**  |
| `cargo test --lib X` rerun (no source)    | 30+ s      | **1.46 s**   |

The headline win is **6 minutes → 1 second** for no-op rebuilds.
Linux hosts already see native ext4 on the bind mount so the speedup
is smaller, but named volumes still beat the bind mount slightly.

### Wiping the warm volumes

If the cargo fingerprint state ever gets corrupted, wipe explicitly.
The names are per-checkout, so ask the script for them rather than
guessing:

```bash
SOLDR_COOK_PRINT_PLAN=1 bench/cook_in_docker.sh
docker volume rm soldr-perf-target-<root> soldr-perf-cargo-home-<root>
```

The next run is a full cold build (~5–8 min) into the fresh volume;
subsequent runs are seconds again.

### Migration from the old layout

After upgrading to this script, the old host-side `target/` directory
under the repo root becomes orphaned (cargo writes into the named
volume instead). Reclaim disk with:

```bash
rm -rf target/
```

## Arbitrary cargo commands via `ci/perf_local.py`

For day-to-day iteration (not just the cook test harness), use the
convenience CLI that runs ANY cargo command against the same warm
volumes:

```bash
uv run --no-project python ci/perf_local.py cargo build --release
uv run --no-project python ci/perf_local.py cargo test --workspace
uv run --no-project python ci/perf_local.py cargo clippy --workspace -- -D warnings

# Volume admin
uv run --no-project python ci/perf_local.py --status   # show volume mount points
uv run --no-project python ci/perf_local.py --wipe     # remove all three perf volumes
```

### Per-checkout isolation

The runner and its three volumes are named after the shared git root:

```
C:\...\dev\soldr   -> soldr-perf-local-soldr-a6c74af0
C:\...\dev\soldr2  -> soldr-perf-local-soldr2-e27990ba
C:\...\dev\soldr3  -> soldr-perf-local-soldr3-ad100fba
```

so sibling checkouts never share or evict each other's runner, and each
keeps its own warm `target/`. Linked worktrees *below* a root still share
that root's runner — only the `docker exec` working directory changes.

Before this, one global `soldr-perf-local` container was shared by every
checkout while the lock was per-root, so starting a run in `soldr2` would
`docker rm -f` a build already running in `soldr`, and all of them fought
over a single Cargo target across different branches.

`bench/cook_in_docker.sh` derives its volume names the same way, for the
same reason: its per-run `docker volume rm --force` of the harness volume
used to destroy a sibling checkout's in-flight state. The two harnesses
keep separate build state — they never shared a container, and no longer
share a target volume either.

`--wipe` only removes the current root's volumes.

`perf_local.py` uses its own `soldr-perf-soldr-home-<root>` volume for
`~/.soldr/` (kept warm, never wiped) so the soldr daemon state and
caches survive across runs. The cook-test harness uses the separate
`cook-soldr-home-<root>` volume that gets wiped per run for test
determinism.

If a `soldr-perf-*` volume with NO root suffix still exists, it predates
this split. A sibling checkout that has not picked up these changes may
still be using it, so check before reclaiming its disk;
`perf_local.py --status` lists any it finds.

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
  `pkg-config`, `libssl-dev`, `git`. Marker env var
  `SOLDR_COOK_DOCKER_HARNESS=1`. `CARGO_HOME=/root/.cargo` pinned so
  the named volume mount point is unambiguous.
- `bench/cook_in_docker.sh` — supported runner for the cook test
  harness. Builds the image, mounts the source tree + three named
  volumes, runs the requested command.
- `ci/perf_local.py` — general-purpose convenience CLI for arbitrary
  cargo commands against the warm volumes (issue #593).
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
