# Darwin cross-compile harness for soldr's release-auto.yml apple-darwin lanes.
#
# Goal: prove a Linux host can produce a Mach-O `soldr` binary for
# x86_64-apple-darwin (and aarch64-apple-darwin) using plain `cargo
# build` + a complete Apple SDK sysroot, NOT `cargo zigbuild`. Once the
# env block in this harness produces a working binary, codify the same
# env into `crates/soldr-cli/src/blessed_build.rs`'s apple-darwin arm.
#
# What's in the image:
#   - rustup 1.94.1 + cross-compile target `x86_64-apple-darwin`
#   - clang-18 / clang++-18 (Ubuntu — has darwin codegen backend built in)
#   - llvm-18 (llvm-ar / llvm-ranlib for Mach-O archives)
#   - zig 0.14.1 (used ONLY as the Mach-O linker, not as the C compiler)
#   - MacOSX11.3.sdk extracted to /opt/apple-sdk/MacOSX11.3.sdk/ — same
#     SHA-pinned bundle soldr's `apple_sdk.rs` fetches in production
#
# Build:
#   docker build -f ci/docker-darwin-cross/Dockerfile -t soldr-darwin-cross .
#
# Run (interactive):
#   docker run --rm -it -v "$PWD:/src" -w /src soldr-darwin-cross bash
#
# Run (build):
#   docker run --rm -v "$PWD:/src" -w /src soldr-darwin-cross \
#     /opt/build-soldr-for-darwin.sh x86_64-apple-darwin
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        xz-utils \
        zstd \
        tar \
        clang-18 \
        clang++-18 \
        llvm-18 \
        cmake \
        make \
        pkg-config \
        git \
        build-essential \
    && ln -sf /usr/bin/clang-18    /usr/bin/clang \
    && ln -sf /usr/bin/clang++-18  /usr/bin/clang++ \
    && ln -sf /usr/bin/llvm-ar-18  /usr/bin/llvm-ar \
    && ln -sf /usr/bin/llvm-ranlib-18 /usr/bin/llvm-ranlib \
    && rm -rf /var/lib/apt/lists/*

# --- zig 0.14.1 (linker only) ---
ARG ZIG_VERSION=0.14.1
RUN curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-x86_64-${ZIG_VERSION}.tar.xz" \
    | tar -xJ -C /opt \
 && ln -s "/opt/zig-linux-x86_64-${ZIG_VERSION}/zig" /usr/local/bin/zig \
 && zig version

# --- Apple SDK (MacOSX11.3.sdk) ---
# Same SHA-pinned URL `crates/soldr-cli/src/fetch/apple_sdk.rs` uses in
# production (MANAGED_APPLE_SDK_URL + MANAGED_APPLE_SDK_SHA256). Treat
# this as the load-bearing image input — the WHOLE point of this
# harness is to use the COMPLETE SDK instead of zig's minimal bundled
# one.
ARG APPLE_SDK_URL=https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/apple-sdk/MacOSX11.3.sdk.tar.xz
ARG APPLE_SDK_SHA256=
RUN mkdir -p /opt/apple-sdk \
 && curl -fsSL "$APPLE_SDK_URL" -o /tmp/MacOSX11.3.sdk.tar.xz \
 && if [ -n "$APPLE_SDK_SHA256" ]; then echo "$APPLE_SDK_SHA256  /tmp/MacOSX11.3.sdk.tar.xz" | sha256sum -c -; fi \
 && tar -xJ -C /opt/apple-sdk -f /tmp/MacOSX11.3.sdk.tar.xz \
 && rm /tmp/MacOSX11.3.sdk.tar.xz \
 && test -f /opt/apple-sdk/MacOSX11.3.sdk/usr/include/sys/syscall.h \
 || (echo "ERROR: Apple SDK extraction did not yield sys/syscall.h — the WHOLE harness depends on this header existing" >&2 && exit 2)

# --- rustup + rust 1.94.1 + cross targets ---
ENV CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    PATH=/root/.cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.94.1 --profile minimal \
 && rustup target add x86_64-apple-darwin aarch64-apple-darwin \
 && rustc --version

# --- the env-fix builder script ---
COPY ci/docker-darwin-cross/build-soldr-for-darwin.sh /opt/build-soldr-for-darwin.sh
RUN chmod +x /opt/build-soldr-for-darwin.sh

WORKDIR /src

# Default: build for x86_64-apple-darwin and print the file type.
CMD ["/opt/build-soldr-for-darwin.sh", "x86_64-apple-darwin"]
