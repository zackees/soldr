# Docker base for Linux → Windows x86_64 cross-compile demo.
#
# Uses the upstream `cargo-zigbuild` image — the maintainer's own
# published image bundles a pinned rust, zig, and cargo-zigbuild
# combination that's known to cross-compile cleanly. We layer the
# `x86_64-pc-windows-gnu` rustup target on top.
#
# See https://github.com/rust-cross/cargo-zigbuild for the upstream
# image's contents and supported rust versions.

FROM messense/cargo-zigbuild:0.20.0

# Add the Windows GNU target. cargo-zigbuild uses zig as the linker, so
# we don't need a separate MinGW toolchain on the linux host — zig
# handles PE/COFF emission.
RUN rustup target add x86_64-pc-windows-gnu

WORKDIR /work

# Default to the cross-compile invocation. The caller can override the
# trailing args (e.g. `--bin <name>`, `--profile=release`, etc.) by
# passing them after the image name on `docker run`.
ENTRYPOINT ["cargo", "zigbuild"]
CMD ["--target", "x86_64-pc-windows-gnu", "--release"]
