# Zero-deps baseline image (glibc) for soldr bootstrap acceptance test.
#
# This image MUST NOT pre-install any build tool that soldr is meant to
# fetch at runtime. The whole point of this acceptance test (issue #860,
# meta #853) is to prove that a stripped image with only the absolute
# minimum (CA roots, curl, git, pkg-config, runtime shared libs) can still
# successfully run:
#
#   soldr cargo build --target x86_64-unknown-linux-gnu
#   soldr cargo zigbuild --target aarch64-apple-darwin
#   soldr cargo xwin build --target x86_64-pc-windows-msvc
#
# DO NOT add `build-essential`, `clang`, `lld`, `llvm`, `zig`, `rustup`,
# `rustc`, `gcc`, or any other compiler / linker toolchain here. Any
# missing piece must come from soldr's fetch path (#841 zig, #862 Apple
# SDK, #864 LLVM, plus rustup/zccache bootstraps).
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get \
        -o Acquire::Retries=5 \
        -o Acquire::http::Timeout=30 \
        -o Acquire::https::Timeout=30 \
        update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        libxml2 \
        pkg-config \
        xz-utils \
    && rm -rf /var/lib/apt/lists/*

# soldr binary is copied in by the workflow after it cross-compiles the
# x86_64-unknown-linux-gnu artifact via `soldr cargo zigbuild`.
RUN mkdir -p /opt/soldr/bin
COPY soldr /opt/soldr/bin/soldr
RUN chmod +x /opt/soldr/bin/soldr

ENV PATH=/opt/soldr/bin:$PATH
# Keep soldr's cache out of /root so subsequent `docker run` invocations
# in CI can swap in a tmpfs / volume cleanly.
ENV SOLDR_HOME=/var/lib/soldr
RUN mkdir -p "$SOLDR_HOME"

WORKDIR /work
