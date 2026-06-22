# One-image linux x86 cross-compile for all 8 targets in
# [workspace.metadata.soldr].targets.
#
# Image layers:
#   1. debian + base build tools
#   2. soldr (via pip — official maturin-built linux x86_64 wheel)
#   3. soldr bootstrap → rustup install
#   4. soldr toolchain install → pin to 1.94.1
#   5. soldr prepare --target all → bake in zig, LLVM, Apple SDK,
#      xwin MSVC CRT cache, every rustup target. ~1.5 GiB but every
#      subsequent `docker run` skips the warm-up entirely.
#
# Replaces the per-OS GH Actions matrix with one local-docker step.
# See examples/docker-cross-all/README.md for the rationale.

FROM --platform=linux/amd64 debian:bookworm-slim

# Base toolchain for soldr + rustup + zig + Apple SDK extraction.
# clang/lld are pulled in so cargo-xwin's MSVC link step works.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git build-essential pkg-config \
        python3 python3-pip python3-venv pipx \
        clang lld llvm zstd xz-utils \
    && rm -rf /var/lib/apt/lists/*

# Install soldr from PyPI. The maturin wheel ships the native binary
# under `soldr._native` — pipx makes the `soldr` CLI globally callable.
ARG SOLDR_VERSION=0.7.59
ENV PIPX_HOME=/opt/pipx PIPX_BIN_DIR=/usr/local/bin
RUN pipx install --pip-args="--no-cache-dir" "soldr==${SOLDR_VERSION}" \
    && soldr --version

# Bootstrap rustup and install the pinned channel. soldr bootstrap is
# idempotent so re-running during cache invalidation is safe.
ENV CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:$PATH
RUN soldr bootstrap
COPY rust-toolchain.toml /tmp/rust-toolchain.toml
RUN cd /tmp && soldr toolchain install

# Prepare the cross-compile toolchain for every target declared under
# [workspace.metadata.soldr].targets in this image's seed Cargo.toml.
# Bakes zig, LLVM, Apple SDK, xwin CRT, and `rustup target add` for
# all 8 triples into the image. `docker run` arrives warm.
COPY seed/Cargo.toml /tmp/Cargo.toml
RUN cd /tmp && soldr prepare --target all

# Workdir is bind-mounted by the host build.sh.
WORKDIR /work

ENTRYPOINT ["/bin/bash"]
