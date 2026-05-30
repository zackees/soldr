# Docker image used to build and test the cross-repo shared `soldr cook`
# artifact cache feature (issues #576, #577, #578 under meta #579).
#
# Why Docker is mandatory for this work: PRs 1-3 mutate the soldr daemon
# protocol, the `~/.soldr/state.redb` schema, and the cargo-front-door
# hot path. soldr installs as a per-user singleton — running `cargo
# build` or `cargo test` for these PRs on the host would corrupt the
# host developer's running daemon and persistent state. Mounting
# `~/.soldr/` as a container-local volume (tmpfs or named volume — NEVER
# a bind-mount of the host path) keeps the host singleton intact.
#
# Usage (via bench/cook_in_docker.sh):
#   docker build -f docker/cook-shared-cache/Dockerfile -t soldr-cook-dev .
#   docker run --rm -v "$(pwd):/work" -v cook-soldr-home:/root/.soldr \
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
    && rm -rf /var/lib/apt/lists/*

# soldr's `cargo test` jobs check the system rustc version; pin it here
# to match `rust-toolchain.toml` (Rust 1.94.1).
RUN rustup default 1.94.1 \
    && rustup component add rustfmt clippy

# Source tree is mounted by the runner at /work; no COPY here so each
# `docker run` reuses the cached image but operates on the live worktree.
WORKDIR /work

# Marker env var read by tests gated on the Docker harness. Bare-host
# `cargo test` runs are no-ops for these gated tests; only `docker run`
# with this image enables them.
ENV SOLDR_COOK_DOCKER_HARNESS=1

# Default command is a help message; the runner overrides with the
# actual `cargo test` invocation.
CMD ["echo", "soldr-cook-dev image — invoke via bench/cook_in_docker.sh or pass a cargo command"]
