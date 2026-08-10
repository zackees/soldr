# Docker Linux dev/test runner

Iterative Docker Linux container for running-process Rust + Python development.
Built for the broker v1 redesign work (#464, #466) so all Linux-specific
behavior — futex, `shm_open`, eventfd, signal semantics — is exercised
directly without touching the Windows host's toolchain or cargo state.

Coexists with the CI-artifact Dockerfiles at repo root
(`Dockerfile.linux-build`, `Dockerfile.linux-lint`, `Dockerfile.linux-pytest`).
Those produce release wheels and CI lint/test containers and are not affected
by this runner.

## Quick start

```bash
# One-time: build the dev image (~5–10 min cold; cached thereafter).
python ci/dev_docker.py build-image

# Compile the workspace.
python ci/dev_docker.py cargo build --release

# Run the repo entrypoints inside the container.
python ci/dev_docker.py test
python ci/dev_docker.py lint
python ci/dev_docker.py wheel

# Interactive shell.
python ci/dev_docker.py shell

# Arbitrary command (everything after `--` is run inside the container).
python ci/dev_docker.py -- env
python ci/dev_docker.py -- soldr cargo nextest run -p running-process
```

## What lives where

The driver mounts five named Docker volumes for build/dependency state and
bind-mounts the source tree at `/work`. Everything that's compile-state-shaped
(slow to rebuild, fingerprint-sensitive) lives in a named volume. Source is
the only bind mount.

| Volume                          | Mount point         | What it holds                              |
| ------------------------------- | ------------------- | ------------------------------------------ |
| `running-process-dev-target`    | `/work/target`      | `CARGO_TARGET_DIR` — cargo build artifacts |
| `running-process-dev-cargo`     | `/usr/local/cargo`  | `CARGO_HOME` — registry, git db, fingerprints |
| `running-process-dev-rustup`    | `/usr/local/rustup` | `RUSTUP_HOME` — installed toolchains       |
| `running-process-dev-uv`        | `/uv`               | `UV_PROJECT_ENVIRONMENT` + `UV_CACHE_DIR`  |
| bind mount                       | `/work`             | Source tree (live, read-write)             |

soldr is intentionally **not** installed in the dev image. The host's
`force_soldr.py` PreToolUse hook is a host-scope policy — it sees the
`docker run ...` invocation from the host, not the `cargo` call running
inside the container. Direct cargo via the `CARGO_TARGET_DIR` volume
gives us the mtime-fingerprint caching benefit we actually need without
soldr's zccache integration breaking on session-start inside fresh
containers. The repo's Python entrypoints (`ci.install` / `ci.test` /
`ci.lint` / `build.py`) all detect `shutil.which("soldr")` and fall
through to direct cargo when soldr is absent, so `./test` / `./lint` /
`uv run build.py` work end-to-end inside the container without
modification.

Why `UV_PROJECT_ENVIRONMENT` is redirected: by default `uv sync` creates
`.venv/` inside the project. With source bind-mounted that means Linux
ELF binaries appear in the host's checkout, which confuses host tooling
and collides with the host's own `.venv/`. The redirect puts the venv
inside a named volume instead.

## Why named volumes (the hard rule)

On Windows + Docker Desktop, host bind mounts pass through WSL2's 9P
translation layer, which rewrites file mtimes on every container start.
Cargo's incremental fingerprint check compares mtimes — when mtimes
shift, cargo rebuilds the world.

**Concrete impact**: with a host bind mount for `target/`, a no-op
`cargo build --release` rebuilds the entire 21-crate workspace (~minutes).
With a named volume, the same no-op build finishes in seconds. The 100×+
ratio is the entire reason this runner exists.

## Validation

Cold/warm fingerprint sanity check — run after any image rebuild to
confirm the named-volume strategy survives container restart:

```bash
python ci/dev_docker.py --wipe                            # start cold
time python ci/dev_docker.py cargo build --release        # cold: minutes
time python ci/dev_docker.py cargo build --release        # warm: seconds
touch crates/running-process/src/lib.rs
time python ci/dev_docker.py cargo build --release        # one crate + deps
```

If the second run rebuilds the world, something is wrong — most likely a
host bind mount has snuck in where a named volume belongs, or
`CARGO_TARGET_DIR` is unset and cargo wrote into a temp path the next run
can't see.

Full repo entrypoint coverage:

```bash
python ci/dev_docker.py test       # Rust nextest + Python pytest
python ci/dev_docker.py lint       # ruff + black + isort + pyright + KBI
python ci/dev_docker.py wheel      # uv run build.py (dev wheel)
```

## Known side effects

- **`uv.lock` may be touched** when running `./test` or `./lint` inside the
  container, because those scripts call `uv sync --refresh --no-editable`
  which re-resolves the lock. The source tree is bind-mounted read-write,
  so the modification reaches the host. Revert with
  `git checkout -- uv.lock` before committing.
- **`dist/`** receives the built wheel from `python ci/dev_docker.py wheel`.
  Gitignored by default. Safe to leave.
- **`logs/`** may receive timeout diagnostics from
  `running_process.cli --timeout` if a build step exceeds an internal idle
  budget. Gitignored. Safe to leave.

## Known test-harness issues

`python ci/dev_docker.py test` runs the full Rust + Python suite. The
following are pre-existing harness assumptions surfaced (not introduced)
by running tests inside a container; the Docker runner workarounds are
documented:

- **`/etc/machine-id`** is missing in vanilla Debian containers. The
  broker's user-identity derivation and the `doctor` diagnostic both
  read it; absence trips `platform:path-budget` and adjacent doctor
  tests. The Dockerfile synthesizes a stable 128-bit hex value at image
  build time. Workaround documented in the Dockerfile.
- **`XDG_RUNTIME_DIR`** is normally set per-session by systemd; the
  broker's runtime-dir derivation falls back without it and trips
  `sockets:runtime-dir`. The image sets `XDG_RUNTIME_DIR=/run/user/0`
  and creates the directory.
- **`gdb`** is installed in the image so the test-watchdog
  (`crates/test-watchdog`) can capture all-thread backtraces when a Rust
  integration test hangs. Without gdb, watchdog kills the test with no
  diagnostics.
- **`containment_test::test_contained_group_kills_grandchildren`** is
  observed to hang intermittently under nextest's default concurrency
  inside the container ("Blocking waiting for file lock on artifact
  directory" — cargo artifact-lock contention between test workers).
  Pre-existing harness sensitivity; not introduced by this runner. Track
  in a follow-up issue if it becomes a recurring blocker.

## Volume management

```bash
python ci/dev_docker.py --status   # show volume mountpoints
python ci/dev_docker.py --wipe     # remove all dev volumes (next run = cold)
```

`--wipe` is the recovery recipe when the cache gets confused (e.g., after a
toolchain pin bump, since Docker only populates volumes from the image on
first mount).

## Constraints and design notes

- **No `COPY` of source code into the Dockerfile.** Source is a volume mount;
  every edit on the host is immediately visible inside the container without
  layer-cache invalidation.
- **soldr is intentionally NOT installed** (see the table above for rationale).
  The host's PreToolUse hook is a host-scope policy and does not constrain
  inside-container commands. soldr 0.7.55's zccache integration also fails
  reliably inside fresh containers with a "private daemon cache dir mismatch"
  on session-start; direct cargo via the named volume sidesteps the issue.
- **No `cargo clean`** is ever invoked by the image or driver — that would
  wipe the volume-mounted `target/`.
- **No default `-it`**: Git-Bash on Windows fools `isatty()` (mintty isn't
  a real ConPTY) and `docker run -it` would fail with "the input device is
  not a TTY". The `shell` subcommand opts in explicitly; arbitrary commands
  can opt in via `CLUD_DOCKER_TTY=1`.
- **`MSYS_NO_PATHCONV=1`** is set on every subprocess invocation so Git-Bash
  doesn't rewrite `/work` into a Windows path before `docker run` sees it.
- **`--init`** is always passed to `docker run` so Ctrl-C in interactive
  shells / `cargo test` propagates cleanly and PID 1 reaps zombies.

## Relationship to the CI artifact images

Three CI-shaped Dockerfiles at repo root remain unchanged:

- `Dockerfile.linux-build` — multi-stage release-wheel builder (one-shot
  `docker build`, artifacts copied out via `docker cp`).
- `Dockerfile.linux-lint` — image for the lint job.
- `Dockerfile.linux-pytest` — alpine pytest runner for the test job.

Those serve CI's build-once-run-once shape. This file (`docker/dev/Dockerfile`)
plus `ci/dev_docker.py` serve interactive iterative development — different
goals, different image shapes, both useful.
