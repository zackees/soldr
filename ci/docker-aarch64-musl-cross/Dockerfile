# Simulate soldr's release-auto.yml Linux ARM64 (musl) build lane in a
# vanilla Ubuntu 24.04 container. Cross-compiles soldr to
# `aarch64-unknown-linux-musl` from an x86_64 host using
# `cargo zigbuild` (zig 0.13.0 bundles a hermetic musl toolchain — no
# musl.cc or other third-party CDN dependency at build time).
#
# Why zigbuild and not musl.cc's aarch64-linux-musl-cross?
#   - musl.cc has been a single-point-of-failure twice on 2026-06-28
#     (release-pipeline runs lost ~14 min each to outages).
#   - musl.cc's `aarch64-linux-musl-cross.tgz` ships 32-bit i386 host
#     binaries. GHA's ubuntu-24.04 evidently cannot exec them (release
#     run #28340749999 — ring's build.rs hit ENOEXEC `os error 8` when
#     cc-rs tried to invoke `aarch64-linux-musl-gcc -E`). Local Docker
#     Desktop / WSL2 kernels happen to support IA32, masking the issue.
#   - cargo-zigbuild is hermetic: zig brings its own x86_64-native musl
#     toolchain that does not depend on host kernel features.
#
# Catalogue-first / upstream-fallback (soldr#1029):
#   - zig: fetched via soldr-toolchain catalogue
#     (raw.githubusercontent.com/zackees/soldr-toolchain/assets/zig/manifest.json)
#     with ziglang.org as fallback. The catalogue is the source of truth
#     for URL + SHA256; falling back to the hardcoded ziglang.org URL
#     keeps the harness functional if soldr-toolchain CDN is briefly
#     unreachable, but the verified-SHA path runs through the catalogue.
#   - cargo-zigbuild: installed via `cargo install` for now. A future
#     pass will route this through the soldr-toolchain catalogue too
#     (cargo-zigbuild's entry already exists at
#     soldr-toolchain/assets/cargo-zigbuild/manifest.json).
#
# Purpose (per user directive 2026-06-28): all soldr cross-compile
# changes that touch the release pipeline MUST be validated locally
# in this docker before being pushed to GitHub.
#
# Build:
#   docker build -f ci/docker-aarch64-musl-cross/Dockerfile \
#       -t soldr-aarch64-musl-cross .
#
# Build with simulated musl.cc + soldr-toolchain catalogue outage:
#   docker build --add-host musl.cc:127.0.0.1 \
#       --add-host raw.githubusercontent.com:127.0.0.1 \
#       -f ci/docker-aarch64-musl-cross/Dockerfile \
#       -t soldr-aarch64-musl-cross:fallback-test .
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
# lane has access to today.
#   curl + ca-certificates  → toolchain + rustup downloads
#   git                     → cargo fetch from git deps
#   xz-utils + bzip2        → tar extraction for zig + release archives
#   build-essential         → host cc/ld for build scripts
#   pkg-config              → many *-sys crates poke pkg-config at build time
#   file                    → final verification of the cross-compile output
#   jq                      → JSON parsing for catalogue-first toolchain fetch
#   musl-tools              → musl-gcc for the HOST arch (x86_64-musl native)
#   cmake + perl            → zstd-sys (cmake) and openssl-sys (perl) build
#                             scripts. Pre-installed on GHA's ubuntu-latest
#                             but not in a vanilla ubuntu:24.04 base.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl git xz-utils bzip2 \
        build-essential pkg-config file jq musl-tools \
        cmake perl \
 && rm -rf /var/lib/apt/lists/*

# Install zig via the soldr-toolchain catalogue (with ziglang.org
# fallback). Provides the hermetic x86_64-host musl toolchain that
# cargo-zigbuild orchestrates for the aarch64-unknown-linux-musl target.
#
# Pinned at zig 0.13.0 for now (the known-good version validated by
# soldr#1029's initial pivot). Bumping to 0.15.2 is a follow-up after
# the lane is stable.
ENV ZIG_VERSION=0.13.0 \
    ZIG_CATALOGUE_URL=https://raw.githubusercontent.com/zackees/soldr-toolchain/assets/zig/manifest.json \
    ZIG_FALLBACK_URL=https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz \
    ZIG_FALLBACK_SHA256=d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea

RUN set -eu; \
    echo "resolving zig ${ZIG_VERSION} via soldr-toolchain catalogue: ${ZIG_CATALOGUE_URL}"; \
    zig_url=""; \
    zig_sha=""; \
    if curl -fsSL --connect-timeout 15 --max-time 30 \
            "$ZIG_CATALOGUE_URL" -o /tmp/zig-manifest.json; then \
        zig_url=$(jq -r --arg v "$ZIG_VERSION" '.releases[] | select(.version==$v) | .platforms[] | select(.platform.os=="linux" and .platform.arch=="x86_64") | .asset.urls[0]' /tmp/zig-manifest.json); \
        zig_sha=$(jq -r --arg v "$ZIG_VERSION" '.releases[] | select(.version==$v) | .platforms[] | select(.platform.os=="linux" and .platform.arch=="x86_64") | .asset.sha256' /tmp/zig-manifest.json); \
        if [ -z "$zig_url" ] || [ "$zig_url" = "null" ]; then \
            echo "catalogue resolved but ${ZIG_VERSION} not in manifest; falling back" >&2; \
            zig_url=""; \
        fi; \
        rm -f /tmp/zig-manifest.json; \
    else \
        echo "catalogue unreachable; falling back to hardcoded ziglang.org URL" >&2; \
    fi; \
    if [ -z "$zig_url" ]; then \
        zig_url="$ZIG_FALLBACK_URL"; \
        zig_sha="$ZIG_FALLBACK_SHA256"; \
    fi; \
    echo "fetching zig from: $zig_url"; \
    curl -fsSL --connect-timeout 30 --max-time 600 --retry 5 \
        "$zig_url" -o /tmp/zig.tar.xz; \
    echo "${zig_sha}  /tmp/zig.tar.xz" | sha256sum -c -; \
    mkdir -p /opt; \
    tar -xJf /tmp/zig.tar.xz -C /opt; \
    rm /tmp/zig.tar.xz; \
    zig_dir=$(ls -d /opt/zig-* | head -1); \
    ln -s "${zig_dir}/zig" /usr/local/bin/zig; \
    zig version

# Stock rustup — installs the pinned toolchain from rust-toolchain.toml
# automatically when cargo first runs in /src.
RUN curl -fsSL https://sh.rustup.rs | sh -s -- \
        --default-toolchain none -y \
 && /root/.cargo/bin/rustup --version

ENV PATH=/root/.cargo/bin:$PATH \
    RUSTUP_HOME=/root/.rustup \
    CARGO_HOME=/root/.cargo

# Pre-install cargo-zigbuild so the per-build step doesn't need to
# fetch from crates.io. Pinned to a known-working version.
RUN /root/.cargo/bin/rustup toolchain install stable --profile minimal --no-self-update \
 && /root/.cargo/bin/cargo +stable install cargo-zigbuild --version 0.19.7 --locked \
 && cargo-zigbuild --version

# Kernel-independent 32-bit ELF guard (soldr#1029 agent option C).
# Detects toolchain binaries that ship as 32-bit i386 — these run on
# Docker Desktop's WSL2 kernel but ENOEXEC on GHA's azure kernel where
# CONFIG_IA32_EMULATION_DEFAULT_DISABLED=y. Fail at image-build time
# so the harness can never serve "works locally, fails on GHA" again.
# Scope: /opt is the only place toolchains land; /root/.cargo/bin is
# rustup-managed (x86_64) and exempt.
RUN echo "scanning /opt for 32-bit ELF binaries (musl.cc-style i386 traps)..." \
 && bad_files=$(find /opt -type f -executable -exec file {} + 2>/dev/null | awk '/ELF 32-bit/ {print $1}' | sed 's/:$//') \
 && if [ -n "$bad_files" ]; then \
        echo "FATAL: 32-bit ELF binaries found in /opt — would ENOEXEC on GHA azure kernel:" >&2; \
        echo "$bad_files" >&2; \
        exit 1; \
    fi \
 && echo "OK: no 32-bit ELF binaries found in /opt"
