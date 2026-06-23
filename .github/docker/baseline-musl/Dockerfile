# Zero-deps baseline image (musl) for soldr bootstrap acceptance test.
#
# Sister image to baseline-glibc/Dockerfile. Same hard rule: NO
# pre-installed build toolchain. Only enough to let soldr's fetch path
# do its job (CA roots, curl for HTTPS, git for VCS-aware fetches,
# bash for the test shell harness, and libgcc for soldr-managed
# musl prebuilts).
#
# DO NOT add `build-base`, `clang`, `lld`, `llvm`, `zig`, `rustup`,
# `rustc`, `gcc`, `musl-dev`-as-a-build-toolchain, or any other
# compiler / linker. Anything missing must come from soldr's fetch
# path (#841 zig, #862 Apple SDK, #864 LLVM, plus rustup/zccache).
#
# musl coverage matters because glibc and musl behave differently for
# dynamic loader resolution, getaddrinfo behavior, and a number of
# fetched prebuilt-binary compatibility edges (issue #808 picked the
# host-libc-matching target triple specifically because of musl).
FROM alpine:3.20

RUN apk add --no-cache --timeout 30 \
        ca-certificates \
        curl \
        git \
        libgcc \
        bash \
        xz

# soldr binary is copied in by the workflow after it cross-compiles the
# x86_64-unknown-linux-musl artifact via `soldr cargo zigbuild`.
RUN mkdir -p /opt/soldr/bin
COPY soldr /opt/soldr/bin/soldr
RUN chmod +x /opt/soldr/bin/soldr

ENV PATH=/opt/soldr/bin:$PATH
ENV SOLDR_HOME=/var/lib/soldr
RUN mkdir -p "$SOLDR_HOME"

WORKDIR /work
