# One-image linux x86 cross-compile for the full soldr target matrix.
#
# Image layers:
#   1. debian + base build tools
#   2. soldr (via pipx — official maturin-built linux x86_64 wheel)
#   3. soldr bootstrap → rustup install
#   4. soldr toolchain install → pin to rust 1.94.1
#   5. soldr prepare --target <comma-separated-list> → bake in zig,
#      LLVM, Apple SDK, xwin MSVC CRT cache, every rustup target.
#      ~1.5 GiB but every subsequent `docker run` skips the warm-up.
#
# At image-build time no source volume is mounted, so `--target all`
# (which reads `[workspace.metadata.soldr].targets` from a workspace
# `Cargo.toml`) can't be used. The explicit comma-separated list form
# ships in soldr PR #925 — bumping `SOLDR_VERSION` to a release that
# includes #925 is what unblocks this Dockerfile.
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

# Prepare the cross-compile toolchain for every target in one shot.
# Keep this list in sync with the root `[workspace.metadata.soldr]`
# entry of soldr's own Cargo.toml — the explicit comma-separated form
# is used here because there is no Cargo.toml mounted yet at image-
# build time. Requires soldr PR #925.
RUN soldr prepare --target \
x86_64-pc-windows-msvc,aarch64-pc-windows-msvc,x86_64-apple-darwin,aarch64-apple-darwin,x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu,x86_64-unknown-linux-musl,aarch64-unknown-linux-musl

# Workdir is bind-mounted by the host build.sh.
WORKDIR /work

ENTRYPOINT ["/bin/bash"]
