# Linux → Windows x64 wheel builder, driven by a released soldr from PyPI.
#
# Purpose: produce an installable win-x64 soldr wheel so a wedged host can be
# unwedged with a known-good build. It is a test/recovery harness, not a
# release path -- the wheel is built at the dev profile, which is what makes a
# warm rebuild fast. Release wheels come from `release-auto.yml`.
#
# The image does almost all of the work at BAKE time: Rust toolchain, the
# Windows std, the blessed MSVC SDK/sysroot, and maturin all land in image
# layers. Run-time named volumes then carry incremental state between runs, so
# a warm rebuild compiles changed crates and nothing else.
#
# soldr's own `prepare --help` calls out this exact shape: a comma-separated
# `--target` list is "useful for docker-image bake steps where no workspace
# `Cargo.toml` is mounted yet".

FROM python:3.13-slim-bookworm

# clang/lld back the MSVC link path; git is needed for the vendored submodules
# and cargo's git dependencies. The apt cache mounts keep a layer rebuild off
# the network.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt-get -o Acquire::Retries=5 update \
    && apt-get -o Acquire::Retries=5 install -y --no-install-recommends \
        ca-certificates \
        clang \
        curl \
        git \
        lld \
        llvm \
        pkg-config \
        xz-utils

# uv provides the fast installer; soldr provisions maturin itself.
COPY --from=ghcr.io/astral-sh/uv:0.9.18 /uv /uvx /usr/local/bin/

# The soldr that DRIVES the build. Pinned deliberately: 0.9.0 is also on PyPI,
# but its broker cannot dial its daemon route, so every cacheable compile
# fails. Bump only after verifying `soldr cargo build` works on the new one.
ARG SOLDR_VERSION=0.8.44
RUN --mount=type=cache,target=/root/.cache/uv \
    uv pip install --system --break-system-packages "soldr==${SOLDR_VERSION}"

ARG WHEEL_TARGET=x86_64-pc-windows-msvc
ARG RUST_TOOLCHAIN=1.95.0
ENV SOLDR_WHEEL_TARGET=${WHEEL_TARGET}

# Bake the toolchain and the target lifecycle. `--allow-unpinned` is required
# and honest here: there is no `rust-toolchain.toml` in the image, so soldr is
# told explicitly that resolving rustc from PATH is intended at bake time. At
# run time the repo is mounted and its pin applies again.
RUN soldr rustup toolchain install "${RUST_TOOLCHAIN}" --profile minimal --no-self-update \
    && soldr rustup default "${RUST_TOOLCHAIN}" \
    && soldr rustup target add "${WHEEL_TARGET}" --toolchain "${RUST_TOOLCHAIN}" \
    && soldr prepare --target "${WHEEL_TARGET}" --allow-unpinned

COPY build_wheel.py /usr/local/bin/build_wheel.py

WORKDIR /work
ENTRYPOINT ["python3", "/usr/local/bin/build_wheel.py"]
