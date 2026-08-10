# Musl x86 unit-test repro harness, modeled on soldr's
# docker/cook-shared-cache/Dockerfile and bench/cook_in_docker.sh.
#
# Why this exists: the CI lane `x86-musl / unit-test (linux-x86-musl)`
# (PR #514) runs `soldr cargo nextest run --target x86_64-unknown-linux-musl`
# on a glibc Ubuntu runner with setup-soldr's cross-compile path. That
# path failed with `can't find crate for std` because setup-soldr's
# managed rustup-home doesn't have the musl rust-std target installed.
#
# Locally we cannot easily replicate the exact setup-soldr managed-
# rustup-home behavior, but we CAN test "does the workspace's
# `--exclude running-process-py` musl build + nextest run actually
# succeed under a musl Rust toolchain?" by running on Alpine, where
# the HOST triple is already x86_64-unknown-linux-musl and no
# `rustup target add` is needed.
#
# This image mirrors soldr's pattern:
#   - Rust toolchain pinned to 1.94.1 (matches rust-toolchain.toml).
#   - soldr installed in the container so `soldr cargo …` actually
#     resolves to the managed rustup toolchain, exactly as CI does.
#   - CARGO_HOME / SOLDR_HOME explicit so the runner script can pin
#     named volume mount points unambiguously.
#   - WORKDIR /work; source is bind-mounted by the runner script.
#   - Default CMD is a help message — the runner overrides with the
#     actual `soldr cargo nextest run` invocation.

FROM rust:1.94.1-alpine

# Build tools needed by the workspace (linker, pkg-config, openssl-dev
# for build scripts, protoc for the proto build, cmake for some crates,
# linux-headers for syscall-using crates).
RUN apk add --no-cache \
    bash \
    build-base \
    musl-dev \
    pkgconf \
    openssl-dev \
    protobuf-dev \
    cmake \
    perl \
    git \
    ca-certificates \
    curl \
    wget \
    linux-headers

# Workaround for zackees/soldr#806: soldr picks the
# `*-unknown-linux-gnu` cargo-nextest binary even on musl hosts, and
# that glibc-linked binary cannot run on Alpine (musl libc only).
# Alpine's `gcompat` shim is too minimal — cargo-nextest needs
# `__res_init` from libresolv which gcompat doesn't provide.
#
# Install the actual glibc via the well-known sgerrand/alpine-pkg-glibc
# package (the standard Alpine-side glibc-compat for cases like this).
# Drop this whole block once soldr#806 ships a host-libc-aware picker.
#
# The default sgerrand `apk add` leaves `/lib/ld-linux-x86-64.so.2`
# as a small ~22 KB wrapper that bounces requests through musl's
# loader — which then fails on glibc-only symbols like `__res_init`.
# Replace it with a direct symlink to glibc's actual loader at
# `/usr/glibc-compat/lib/ld-linux-x86-64.so.2`, and prepend the glibc
# lib dir to `LD_LIBRARY_PATH` so the loader finds glibc's libc.so.6 /
# libresolv-merged symbols.
ARG GLIBC_VER=2.35-r1
RUN wget -qO /etc/apk/keys/sgerrand.rsa.pub https://alpine-pkgs.sgerrand.com/sgerrand.rsa.pub \
 && wget -q "https://github.com/sgerrand/alpine-pkg-glibc/releases/download/${GLIBC_VER}/glibc-${GLIBC_VER}.apk" \
         "https://github.com/sgerrand/alpine-pkg-glibc/releases/download/${GLIBC_VER}/glibc-bin-${GLIBC_VER}.apk" \
 && apk add --no-cache --force-overwrite "glibc-${GLIBC_VER}.apk" "glibc-bin-${GLIBC_VER}.apk" \
 && rm -f glibc-*.apk \
 && /usr/glibc-compat/sbin/ldconfig \
 && ln -sf /usr/glibc-compat/lib/ld-linux-x86-64.so.2 /lib/ld-linux-x86-64.so.2 \
 && ln -sf /usr/glibc-compat/lib/ld-linux-x86-64.so.2 /lib64/ld-linux-x86-64.so.2

ENV LD_LIBRARY_PATH=/usr/glibc-compat/lib:/usr/glibc-compat/lib64

# `/etc/machine-id` is missing on Alpine by default. The running-process
# broker v2 client (`user_sid_hash` in `crates/running-process/src/broker/client_v2.rs`)
# panics on lookup if neither `/etc/machine-id` nor
# `/var/lib/dbus/machine-id` exists. CI runners ship one out of the box;
# Alpine images don't. Stamp a deterministic value here so the broker
# tests run end-to-end inside this image.
RUN printf '%s\n' '00000000000000000000000000000000' > /etc/machine-id

# Pin Rust toolchain explicitly (`rust:1.94.1-alpine` already ships
# 1.94.1, but the +components add is idempotent and future-proof if
# the base image bumps).
RUN rustup default 1.94.1 \
 && rustup component add rustfmt clippy

# Install soldr from its prebuilt musl release tarball at
# https://github.com/zackees/soldr/releases (the crates.io `soldr`
# package is a library — the binary lives in `soldr-cli`, which
# isn't published, so prebuilt is the canonical path). The
# `x86_64-unknown-linux-musl` asset is statically linked and works
# on Alpine without glibc.
ARG SOLDR_VERSION=0.7.56
RUN curl -fsSL "https://github.com/zackees/soldr/releases/download/v${SOLDR_VERSION}/soldr-v${SOLDR_VERSION}-x86_64-unknown-linux-musl.tar.zst" \
        -o /tmp/soldr.tar.zst \
 && apk add --no-cache zstd \
 && zstd -d /tmp/soldr.tar.zst -o /tmp/soldr.tar \
 && tar -xf /tmp/soldr.tar -C /tmp \
 && find /tmp -name soldr -type f -executable -exec install -m 0755 {} /usr/local/bin/soldr \; \
 && rm -rf /tmp/soldr.tar.zst /tmp/soldr.tar /tmp/soldr* \
 && soldr --version

# Explicit env so named volume mount points in the runner script are
# unambiguous, and so `soldr cargo …` finds its state under a known
# path that the runner can hot-mount.
ENV CARGO_HOME=/root/.cargo \
    SOLDR_HOME=/root/.soldr \
    CARGO_TARGET_DIR=/work/target

# Source tree is mounted by the runner at /work; no COPY here so each
# `docker run` reuses the cached image but operates on the live
# worktree.
WORKDIR /work

# Default command is a help message; the runner overrides with the
# actual `soldr cargo …` invocation.
CMD ["echo", "running-process-musl-unit-test image — invoke via ci/musl_in_docker.sh"]
