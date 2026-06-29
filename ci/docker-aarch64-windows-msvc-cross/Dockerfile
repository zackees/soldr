# Simulate soldr's cross-compile to `aarch64-pc-windows-msvc` from a
# Linux x86_64 host in a vanilla Ubuntu 24.04 container.
#
# Mirrors `ci/docker-aarch64-musl-cross/` for the Windows MSVC target.
# Triggered by GHA run 28345283384 / job 83967287913 which hit a
# 30-minute hang on the same cross-compile path —
#   "soldr: cargo diagnostic capture timed out after 1800 seconds"
# This harness exists so the same path can be reproduced locally and
# diagnosed with per-line elapsed-second prefixes (run_with_ts.py).
#
# Toolchain stack:
#   - soldr v0.7.66 (the just-released bundle that fixes both #1032
#     bugs: zig 0.14.1 extraction + clang-shim packaging)
#   - clang on PATH (the blessed `soldr build` surface installs
#     soldr-clang-shim ahead of system clang so ring 0.17.x's
#     hardcoded `c.compiler("clang")` routes to clang-cl for MSVC)
#   - cargo-xwin (the legacy `soldr cargo build` surface uses this
#     to download the MS CRT + SDK headers; still installed for
#     comparison runs)
#   - run_with_ts.py wraps every cargo / soldr invocation so each
#     output line carries an elapsed-seconds prefix, making the
#     "stuck step" pattern from GHA observable line-by-line
#
# Build:
#   docker build -f ci/docker-aarch64-windows-msvc-cross/Dockerfile \
#       -t soldr-aarch64-windows-msvc-cross .
#
# Run (from repo root):
#   docker run --rm -v "$PWD:/src" -w /src \
#       soldr-aarch64-windows-msvc-cross \
#       bash ci/docker-aarch64-windows-msvc-cross/build.sh

FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# Minimum apt set — mirrors what GHA's ubuntu-24.04 image provides
# plus the deps soldr / cargo-xwin / ring expect.
#   curl + ca-certificates  → toolchain + soldr release downloads
#   git                     → cargo fetch from git deps
#   xz-utils + bzip2 + zip + unzip → archive plumbing
#   build-essential         → host cc/ld for build scripts
#   pkg-config              → many *-sys crates poke pkg-config
#   file + jq               → diagnostics + catalogue parsing
#   python3-minimal         → run_with_ts.py
#   clang + lld + llvm + llvm-dev → ring's build.rs needs clang-cl
#                             routing; lld for cross-link; llvm-lib
#                             from llvm-dev satisfies cargo-xwin
#   cmake + perl            → zstd-sys + openssl-sys build scripts
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl git xz-utils bzip2 zip unzip \
        build-essential pkg-config file jq python3-minimal \
        clang lld llvm llvm-dev libclang-dev \
        cmake perl \
 && rm -rf /var/lib/apt/lists/*

# Stock rustup — installs the pinned toolchain from rust-toolchain.toml
# on first cargo invocation in /src.
RUN curl -fsSL https://sh.rustup.rs | sh -s -- \
        --default-toolchain none -y \
 && /root/.cargo/bin/rustup --version

ENV PATH=/root/.cargo/bin:$PATH \
    RUSTUP_HOME=/root/.rustup \
    CARGO_HOME=/root/.cargo

# Install soldr v0.7.66 from the just-published GitHub release. This
# is the version that ships the soldr-clang-shim binary alongside
# soldr (the #1032 bug 2 fix), so `soldr build --target
# aarch64-pc-windows-msvc` can install the shim to route ring's
# hardcoded `clang` invocations to clang-cl.
ENV SOLDR_VERSION=0.7.66 \
    SOLDR_ASSET_URL=https://github.com/zackees/soldr/releases/download/v0.7.66/soldr-v0.7.66-x86_64-unknown-linux-gnu.tar.zst

RUN set -eu; \
    echo "fetching soldr ${SOLDR_VERSION}"; \
    curl -fsSL --connect-timeout 30 --max-time 600 --retry 5 \
        "$SOLDR_ASSET_URL" -o /tmp/soldr.tar.zst; \
    apt-get update && apt-get install -y --no-install-recommends zstd \
      && rm -rf /var/lib/apt/lists/*; \
    mkdir -p /opt/soldr; \
    tar --use-compress-program=zstd -xf /tmp/soldr.tar.zst -C /opt/soldr; \
    rm /tmp/soldr.tar.zst; \
    chmod +x /opt/soldr/soldr || true; \
    ls /opt/soldr; \
    /opt/soldr/soldr --version

ENV PATH=/opt/soldr:$PATH

# Pre-install cargo-xwin (the legacy cross-MSVC path) for comparison.
# `soldr build --target *-pc-windows-msvc` will resolve cargo-xwin via
# the catalogue; this baked-in copy avoids a per-build fetch.
RUN /root/.cargo/bin/rustup toolchain install stable --profile minimal --no-self-update \
 && /root/.cargo/bin/cargo +stable install cargo-xwin --version 0.18.6 --locked \
 && cargo-xwin --version

# Kernel-independent 32-bit ELF guard — same paranoia as the
# aarch64-musl harness. Catches future toolchain vendor changes that
# might ship 32-bit binaries that locally work but ENOEXEC on GHA.
RUN echo "scanning /opt /usr/local/bin /root/.cargo/bin for 32-bit ELFs..." \
 && bad_files=$(find /opt /usr/local/bin /root/.cargo/bin -type f -executable -exec file {} + 2>/dev/null | awk '/ELF 32-bit/ {print $1}' | sed 's/:$//') \
 && if [ -n "$bad_files" ]; then \
        echo "FATAL: 32-bit ELF binaries found that would ENOEXEC on GHA azure kernel:" >&2; \
        echo "$bad_files" >&2; \
        exit 1; \
    fi \
 && echo "OK: no 32-bit ELF binaries found"
