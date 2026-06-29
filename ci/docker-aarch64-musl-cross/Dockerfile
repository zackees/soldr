# Simulate soldr's release-auto.yml Linux ARM64 (musl) build lane in a
# vanilla Ubuntu 24.04 container. Cross-compiles soldr to
# `aarch64-unknown-linux-musl` from x86_64 host using musl.cc's
# prebuilt aarch64-linux-musl-cross + a stock rustup toolchain.
#
# Purpose (per user directive 2026-06-28): all soldr cross-compile
# changes that touch the release pipeline MUST be validated locally
# in this docker before being pushed to GitHub. The container needs
# to match what GHA's `ubuntu-latest` provides so a green run here
# means a green run there.
#
# Build:
#   docker build -f ci/docker-aarch64-musl-cross/Dockerfile \
#       -t soldr-aarch64-musl-cross .
#
# Run (from repo root):
#   docker run --rm -v "$PWD:/src" -w /src soldr-aarch64-musl-cross \
#       bash ci/docker-aarch64-musl-cross/build.sh
#
# `build.sh` produces the soldr binary at
# `target/aarch64-unknown-linux-musl/release/soldr` and asserts via
# `file(1)` that it's a `ELF 64-bit LSB ... aarch64` executable.

FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# Minimum apt set — mirrors what release-auto.yml's Linux ARM64 (musl)
# lane has access to today, plus the new aarch64-linux-musl-cross
# install that lives one step before the rustup toolchain setup.
#   curl + ca-certificates  → toolchain + rustup downloads
#   git                     → cargo fetch from git deps
#   xz-utils + bzip2        → tar extraction for various release archives
#   build-essential         → host cc/ld for build scripts
#   pkg-config              → many *-sys crates poke pkg-config at build time
#   file                    → final verification of the cross-compile output
#   musl-tools              → musl-gcc for the HOST arch (x86_64-musl native)
#   cmake + perl            → a transitive *-sys crate in soldr's tree calls
#                            cmake (likely zstd-sys or similar); perl is
#                            needed by openssl-sys when it's pulled. These
#                            are pre-installed on GHA's ubuntu-latest image
#                            but not in a vanilla ubuntu:24.04 base.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl git xz-utils bzip2 \
        build-essential pkg-config file musl-tools \
        cmake perl \
 && rm -rf /var/lib/apt/lists/*

# Download the aarch64-linux-musl-cross toolchain. This is the load-
# bearing piece that makes jemalloc-sys's build script succeed for
# the aarch64-unknown-linux-musl target — without it jemalloc's
# configure script fails atomics detection.
#
# Mirror-first strategy (soldr#1027): musl.cc has been a single-point-
# of-failure twice (2026-06-28 — both release runs that day lost ~14
# min each to musl.cc connectivity timeouts). Try the github.com
# mirror under zackees/soldr-toolchain FIRST; fall back to musl.cc
# upstream only if the mirror is unreachable.
#
# SHA256 pin is for the mirror tarball (provenance: repacked from a
# successful musl.cc pull 2026-06-28). When falling back to musl.cc,
# we accept upstream-as-authoritative without re-verifying since
# musl.cc's own SHA may differ from the mirror's repack — same
# binaries, different tar metadata.
ENV MUSL_CROSS_MIRROR_URL=https://github.com/zackees/soldr-toolchain/releases/download/musl-cross-v1/aarch64-linux-musl-cross.tgz \
    MUSL_CROSS_UPSTREAM_URL=https://musl.cc/aarch64-linux-musl-cross.tgz \
    MUSL_CROSS_MIRROR_SHA256=67f54eb62ee912477a530e70567f82f5c8fb58ce458faafd76a23fbb08a38287

RUN set -eu; \
    echo "fetching aarch64-linux-musl cross from mirror: $MUSL_CROSS_MIRROR_URL"; \
    if curl -fsSL --connect-timeout 30 --max-time 600 --retry 3 \
            "$MUSL_CROSS_MIRROR_URL" -o /tmp/cross.tgz; then \
        echo "mirror download OK; verifying sha256..."; \
        echo "$MUSL_CROSS_MIRROR_SHA256  /tmp/cross.tgz" | sha256sum -c -; \
    else \
        echo "mirror unreachable; falling back to upstream: $MUSL_CROSS_UPSTREAM_URL" >&2; \
        curl -fsSL --connect-timeout 30 --max-time 600 --retry 5 \
            "$MUSL_CROSS_UPSTREAM_URL" -o /tmp/cross.tgz; \
        echo "upstream download OK (sha not verified — upstream-as-authoritative fallback)"; \
    fi; \
    mkdir -p /opt; \
    tar -xzf /tmp/cross.tgz -C /opt; \
    rm /tmp/cross.tgz

ENV PATH=/opt/aarch64-linux-musl-cross/bin:$PATH \
    CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc \
    CXX_aarch64_unknown_linux_musl=aarch64-linux-musl-g++ \
    AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc

# Stock rustup — installs the pinned toolchain from rust-toolchain.toml
# automatically when cargo first runs in /src.
RUN curl -fsSL https://sh.rustup.rs | sh -s -- \
        --default-toolchain none -y \
 && /root/.cargo/bin/rustup --version

ENV PATH=/root/.cargo/bin:$PATH \
    RUSTUP_HOME=/root/.rustup \
    CARGO_HOME=/root/.cargo
