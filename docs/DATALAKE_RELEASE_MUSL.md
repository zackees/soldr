# Datalake-Core Release-Musl Soldr Cache Status

This note tracks the downstream `TechWatchProject/datalake-core` release-musl
workload from soldr#1435.

## Status

Status: fixed / no longer reproducible on `soldr 0.8.0+`.

The old workaround used:

```bash
soldr --no-cache cargo build --release --locked --target x86_64-unknown-linux-musl
```

because a cached release build could previously stall or lose the zccache
daemon during an LTO-heavy musl compile. The exact older failing soldr/zccache
pair was not captured, so the narrowest defensible upstream status is:

- `soldr 0.8.0` with embedded `zccache 1.12.14` passes the downstream
  release-musl workload in a local Docker repro.
- Current soldr release artifacts embed zccache in `soldr-daemon`; Rust and
  native-C compile requests go through the `Request::Compile` IPC path instead
  of the removed external managed-zccache daemon download.
- A recurrence should be treated as a new bug only if it reproduces on
  `soldr 0.8.0+` with cache enabled and includes the soldr daemon/log paths
  described below.

## Downstream Guidance

Downstreams may remove the release-job `--no-cache` workaround after one green
validation run on `soldr 0.8.0+` using the same toolchain, target, and release
profile as production:

```bash
soldr cargo build --release --locked --target x86_64-unknown-linux-musl
```

Keep `soldr --no-cache cargo ...` only for:

- soldr versions older than `0.8.0`
- emergency CI recovery while collecting diagnostics for a new failure
- intentionally uncached release lanes where deterministic no-daemon behavior
  is more important than cache reuse

If a single release/LTO compile request can legitimately take more than 30
minutes before returning a daemon reply, raise that targeted no-response
backstop:

```bash
SOLDR_COMPILE_REPLY_TIMEOUT_SECS=3600 soldr cargo build --release --locked --target x86_64-unknown-linux-musl
```

Normal `soldr cargo ...` builds have no Cargo wall-clock deadline. The
30-minute `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` default above is a targeted IPC
no-response backstop, not a limit on the full build. Do not use `--no-cache`
unless that daemon diagnostic is actually firing.

## Manual Regression Repro

Run this from the downstream repository root after the `core` submodule is
checked out at the revision under test. The repro mirrors the datalake-core
release-musl shape: Rust 1.94.1, musl target, release profile, large dependency
graph, and cached soldr enabled.

```bash
docker build --progress=plain -f - -t datalake-soldr-release-musl-repro . <<'DOCKERFILE'
FROM rust:1.94.1-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates clang cmake curl mold musl-tools \
    perl pkg-config python3 python3-pip \
    && rm -rf /var/lib/apt/lists/*

RUN python3 -m pip install --break-system-packages 'soldr>=0.8.0'

WORKDIR /build
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
RUN rustup target add x86_64-unknown-linux-musl

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo fetch --locked --target x86_64-unknown-linux-musl

COPY src ./src
COPY tests/fixtures ./tests/fixtures

ENV RUSTC_BOOTSTRAP=1
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    soldr cargo build --release --locked --target x86_64-unknown-linux-musl
DOCKERFILE
```

Expected result on `soldr 0.8.0+`: the cached soldr build completes and exports
the Docker image. If the build fails, capture:

```bash
soldr logs paths
soldr daemon status
soldr cache
```

and the full failing compile-dispatch diagnostic.

## Daemon-Death Diagnostic Contract

If the embedded zccache cache daemon becomes unreachable, stops responding, or
dies mid-compile, soldr must fail with an actionable wrapper error rather than
hanging indefinitely or surfacing only a bare compiler exit. The diagnostic
names the `soldr-daemon` embedded zccache cache daemon, includes the IPC
endpoint, and points operators at:

- `soldr logs paths`
- `soldr daemon status`
- `soldr --no-cache cargo ...` / `ZCCACHE_DISABLE=1` for emergency recovery
- `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=<seconds>` for tuning the no-response
  backstop on slow release/LTO builds

The focused unit regression for this contract lives in
`compile_dispatch::tests::compile_dispatch_failure_message_names_daemon_death_and_recovery`.
