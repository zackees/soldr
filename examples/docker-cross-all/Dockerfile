# One-image linux x86 cross-compile for the full soldr target matrix.
#
# Multi-stage build:
#
#   Stage 1 (`soldr-builder`) — minimal debian + rust 1.94.1 + clone
#     soldr at `SOLDR_GIT_REF` (default `main`) + `cargo build --release`
#     of the `soldr` bin. Avoids the PyPI release dependency so this
#     example can ride latest main (which includes #925's comma-
#     separated `--target` form) without waiting for the next pip
#     release.
#
#   Stage 2 (runtime) — debian + cross-link tools + the soldr binary
#     copied from stage 1 + `soldr bootstrap` + `soldr toolchain
#     install` + `soldr prepare --target <8-triple-list>` baking
#     zig, LLVM, Apple SDK, xwin MSVC CRT cache, and every rustup
#     target. ~1.5 GiB but every subsequent `docker run` skips the
#     warm-up entirely.
#
# Override `SOLDR_GIT_REF` (`--build-arg SOLDR_GIT_REF=<sha|branch>`)
# to pin the soldr commit baked into the image.
#
# Replaces the per-OS GH Actions matrix with one local-docker step.
# See examples/docker-cross-all/README.md for the rationale.

# ============================================================
# Stage 1: build soldr from source
# ============================================================
FROM --platform=linux/amd64 debian:bookworm-slim AS soldr-builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

ENV CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain 1.94.1 --profile minimal --no-modify-path

ARG SOLDR_GIT_REF=main
RUN git clone --depth 1 --branch ${SOLDR_GIT_REF} \
        https://github.com/zackees/soldr /soldr-src
RUN cd /soldr-src \
    && cargo build --release --package soldr-cli --bin soldr \
    && /soldr-src/target/release/soldr --version

# ============================================================
# Stage 2: runtime image with cross-toolchain baked in
# ============================================================
FROM --platform=linux/amd64 debian:bookworm-slim

# Cross-link tooling for cargo-xwin's MSVC link step and Apple-SDK
# extraction; zstd/xz for the soldr-fetched tarballs.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git build-essential pkg-config \
        clang lld llvm zstd xz-utils \
    && rm -rf /var/lib/apt/lists/*

COPY --from=soldr-builder /soldr-src/target/release/soldr /usr/local/bin/soldr
RUN soldr --version

# Bootstrap rustup + the pinned channel. `soldr bootstrap` is
# idempotent so re-running on a cache invalidation is safe.
ENV CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:$PATH
RUN soldr bootstrap
COPY rust-toolchain.toml /tmp/rust-toolchain.toml
RUN cd /tmp && soldr toolchain install

# Bake the cross-compile toolchain for every target in one shot.
# Keep this list in sync with the root `[workspace.metadata.soldr]`
# entry of soldr's own Cargo.toml. The explicit comma-separated form
# is used because at image-build time no source volume is mounted —
# `--target all` (which reads `[workspace.metadata.soldr].targets`
# from a workspace `Cargo.toml`) can't see one.
RUN soldr prepare --target \
x86_64-pc-windows-msvc,aarch64-pc-windows-msvc,x86_64-apple-darwin,aarch64-apple-darwin,x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu,x86_64-unknown-linux-musl,aarch64-unknown-linux-musl

# Workdir is bind-mounted by the host build.sh.
WORKDIR /work

ENTRYPOINT ["/bin/bash"]
