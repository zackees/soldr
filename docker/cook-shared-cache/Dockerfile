# Docker image used to build and test the cross-repo shared `soldr cook`
# artifact cache feature (issues #576, #577, #578 under meta #579) and
# the general perf_local convenience CLI (issue #593).
#
# Why Docker is mandatory for cook-shared-cache work: PRs 1-3 mutate
# the soldr daemon protocol, the `~/.soldr/state.redb` schema, and the
# cargo-front-door hot path. soldr installs as a per-user singleton —
# running `cargo build` or `cargo test` for these PRs on the host would
# corrupt the host developer's running daemon and persistent state.
# Mounting `~/.soldr/` as a container-local volume (tmpfs or named
# volume — NEVER a bind-mount of the host path) keeps the host
# singleton intact.
#
# Volume layout (issue #593): see `bench/cook_in_docker.sh` and
# `ci/perf_local.py` for the canonical `docker run -v ...` flags.
# Summary: `/work/target` and `/root/.cargo` are NAMED VOLUMES (not
# host bind mounts) so cargo's mtime-based fingerprint check survives
# container restarts. Without this, Windows + Docker Desktop's WSL2
# 9P translation layer rewrites mtimes per container start and cargo
# rebuilds everything (~6 min on a fresh 21-crate workspace, vs ~1 s
# when warm).
#
# Usage (via bench/cook_in_docker.sh):
#   docker build -f docker/cook-shared-cache/Dockerfile -t soldr-cook-dev .
#   docker run --rm -v "$(pwd):/work" \
#       -v cook-soldr-home:/root/.soldr \
#       -v soldr-perf-target:/work/target \
#       -v soldr-perf-cargo-home:/root/.cargo \
#       -e SOLDR_COOK_DOCKER_HARNESS=1 -w /work soldr-cook-dev \
#       cargo test --workspace -- --include-ignored cook_index

FROM rust:1.94.1-bookworm

# Build tools needed by the soldr-cli workspace and the integration
# tests (linker, pkg-config, git for origin-URL probes in PRs 2/3).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       build-essential \
       pkg-config \
       libssl-dev \
       git \
       ca-certificates \
       curl \
       cmake \
       ninja-build \
       jq \
       zstd \
    && rm -rf /var/lib/apt/lists/*

# soldr's `cargo test` jobs check the system rustc version; pin it here
# to match `rust-toolchain.toml` (Rust 1.94.1).
RUN rustup default 1.94.1 \
    && rustup component add rustfmt clippy

# CARGO_HOME explicit so the `soldr-perf-cargo-home` named volume mount
# point is unambiguous. Without this the path depends on the base
# image's $HOME inheritance — `/root/.cargo` happens to be the default
# for the `rust:bookworm` image but pinning it makes future image
# migrations safe.
ENV CARGO_HOME=/root/.cargo

# Source tree is mounted by the runner at /work; no COPY here so each
# `docker run` reuses the cached image but operates on the live worktree.
WORKDIR /work

# Marker env var read by tests gated on the Docker harness. Bare-host
# `cargo test` runs are no-ops for these gated tests; only `docker run`
# with this image enables them.
ENV SOLDR_COOK_DOCKER_HARNESS=1

# Default command is a help message; the runner overrides with the
# actual `cargo test` invocation.
CMD ["echo", "soldr-cook-dev image — invoke via bench/cook_in_docker.sh or ci/perf_local.py"]
