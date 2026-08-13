# Self-hosting cross-compile validation harness — soldr#1309.
#
# Goal: prove `soldr build --target <T>` provisions the ENTIRE cross
# toolchain itself (LLVM binutils from zackees/clang-tool-chain-bins,
# Apple SDK + cmake/ninja from the soldr-toolchain catalogue) with NO
# host `apt install clang lld llvm`. soldr is a bootstrapping tool: a
# build flow that needs the cross toolchain pre-installed means we're
# not baking/using the right assets.
#
# What this image deliberately DOES NOT contain:
#   - clang / clang++ / lld / ld.lld   (the darwin + msvc linker/driver)
#   - llvm / llvm-ar / llvm-ranlib     (cc-rs archiver — the exact tool
#                                        the pre-fix darwin lane failed
#                                        to find: "cc-rs: failed to find
#                                        tool llvm-ar")
#   - musl-tools                       (musl lanes go through zig-cc)
#   - the Apple SDK                    (soldr fetches it at build time)
#
# What it DOES contain (native host tooling only, for the one-time seed
# build of the soldr driver — the allowed bootstrap):
#   - rustup 1.95.0 + the cross targets
#   - gcc / cmake / make               (native libz-ng-sys etc. for the
#                                        seed's own host build)
#   - curl / zstd / git / ca-certs
#
# Build the image (one-time):
#   docker build -f ci/docker-selfhost-cross/Dockerfile -t soldr-selfhost-cross .
#
# Run a self-hosting cross build (asserts zero cross-toolchain apt):
#   ./ci/docker-selfhost-cross/build.sh aarch64-apple-darwin
#   ./ci/docker-selfhost-cross/build.sh x86_64-pc-windows-msvc
#   ./ci/docker-selfhost-cross/build.sh x86_64-unknown-linux-musl
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# NATIVE host tooling only. No clang / lld / llvm / musl-tools — those
# are what soldr must provision for the cross target. gcc + cmake +
# make are here solely to build the seed soldr driver for the native
# linux-x86_64 host (the one allowed bootstrap).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        xz-utils \
        zstd \
        tar \
        gcc \
        g++ \
        cmake \
        make \
        pkg-config \
        git \
    && rm -rf /var/lib/apt/lists/*

ENV CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    PATH=/root/.cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.95.0 --profile minimal \
 && rustup target add \
        x86_64-unknown-linux-gnu \
        aarch64-unknown-linux-gnu \
        x86_64-unknown-linux-musl \
        aarch64-unknown-linux-musl \
        x86_64-apple-darwin \
        aarch64-apple-darwin \
        x86_64-pc-windows-msvc \
        aarch64-pc-windows-msvc \
 && rustc --version

COPY ci/docker-selfhost-cross/run.sh /opt/run.sh
RUN chmod +x /opt/run.sh

WORKDIR /src
ENTRYPOINT ["/opt/run.sh"]
CMD ["aarch64-apple-darwin"]
