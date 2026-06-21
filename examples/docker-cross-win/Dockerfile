# Docker base for Linux → Windows cross-compile demo (x86_64 + ARM64).
#
# Combines two cross-link toolchains:
#   - cargo-zigbuild (zig as the linker)  — used for GNU/gnullvm targets
#   - cargo-xwin     (clang-cl + lld-link + the MSVC SDK) — used for
#                                          MSVC targets
#
# Built on `messense/cargo-zigbuild` (same author publishes both
# cargo-zigbuild and cargo-xwin). We add cargo-xwin + clang/lld on top
# so one image handles all three lanes the example demonstrates.

FROM messense/cargo-zigbuild:0.20.0

# Three Windows targets covered by this image:
#   - x86_64-pc-windows-gnu        (zigbuild)  — traditional GNU; runs
#                                                 on any Windows x64
#                                                 install, no extra DLLs.
#   - aarch64-pc-windows-msvc      (xwin)      — official MSVC ABI for
#                                                 Windows ARM64. Native
#                                                 Windows tooling and
#                                                 most third-party DLLs
#                                                 use this ABI.
#   - aarch64-pc-windows-gnullvm   (zigbuild)  — same gnullvm family as
#                                                 the x86_64 GNU target,
#                                                 but Windows ARM64.
#                                                 Free with the zigbuild
#                                                 image, no MSVC SDK
#                                                 download required.
RUN rustup target add \
    x86_64-pc-windows-gnu \
    aarch64-pc-windows-msvc \
    aarch64-pc-windows-gnullvm

# clang + lld-link for cargo-xwin's MSVC linking path. Without these,
# `cargo xwin build` fails at the link step.
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang lld llvm \
    && rm -rf /var/lib/apt/lists/*

# cargo-xwin pinned to a version that supports the rust ships with this
# image. The cargo-zigbuild:0.20.0 base bundles rust 1.85; cargo-xwin
# >= 0.23 requires rust 1.89+. 0.19.2 is the latest 1.85-compatible
# release. If you bump the cargo-zigbuild base tag, recompute the
# matching cargo-xwin version with `cargo install cargo-xwin
# --version <X>` — its readme calls out the rust version it needs.
RUN cargo install --locked cargo-xwin@0.19.2

WORKDIR /work

# Generic entrypoint — build.sh dispatches to `cargo zigbuild ...` or
# `cargo xwin build ...` based on the target triple. The CMD here is
# the default no-args invocation that produces the x86_64 lane's
# binary so `docker run <image>` on its own still does something
# useful.
ENTRYPOINT ["cargo"]
CMD ["zigbuild", "--target", "x86_64-pc-windows-gnu", "--release"]
