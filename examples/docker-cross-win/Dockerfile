# Docker base for Linux → Windows MSVC cross-compile demo (x86_64 + ARM64).

FROM rust:1.85-bookworm

# Two Windows MSVC targets covered by this image:
#   - x86_64-pc-windows-msvc       (xwin)      — DEFAULT. The canonical
#                                                 Microsoft ABI for
#                                                 Windows desktop x64.
#                                                 Matches the
#                                                 "MSVC on Windows always"
#                                                 soldr design rule.
#   - aarch64-pc-windows-msvc      (xwin)      — Official MSVC ABI for
#                                                 Windows ARM64. Native
#                                                 Windows tooling and
#                                                 most third-party DLLs
#                                                 use this ABI.
RUN rustup target add \
    x86_64-pc-windows-msvc \
    aarch64-pc-windows-msvc

# clang + lld-link for cargo-xwin's MSVC linking path. Without these,
# `cargo xwin build` fails at the link step.
RUN apt-get \
        -o Acquire::Retries=5 \
        -o Acquire::http::Timeout=30 \
        -o Acquire::https::Timeout=30 \
        update \
    && apt-get \
        -o Acquire::Retries=5 \
        -o Acquire::http::Timeout=30 \
        -o Acquire::https::Timeout=30 \
        install -y --no-install-recommends \
        clang lld llvm \
    && rm -rf /var/lib/apt/lists/*

# cargo-xwin pinned to a version that supports the rust ships with this
# image. cargo-xwin >= 0.23 requires rust 1.89+. 0.19.2 is the latest
# 1.85-compatible release. If you bump the Rust base image, recompute
# the matching cargo-xwin version with `cargo install cargo-xwin
# --version <X>` — its readme calls out the rust version it needs.
RUN CARGO_NET_RETRY=5 CARGO_HTTP_TIMEOUT=30 cargo install cargo-xwin@0.19.2

WORKDIR /work

# Generic entrypoint — build.sh dispatches to `cargo xwin build ...`.
# The CMD here is the default no-args invocation that produces the x86_64-msvc lane's
# binary so `docker run <image>` on its own still does something
# useful — MSVC is the canonical Windows ABI per soldr's
# "MSVC on Windows always" design rule.
ENTRYPOINT ["cargo"]
CMD ["xwin", "build", "--target", "x86_64-pc-windows-msvc", "--release"]
