# Self-contained reproduction harness for zackees/soldr#424:
#
#   `soldr update-zccache <path>` reports a successful path-pin, but a
#   subsequent `soldr cargo build` spawns the MANAGED v1.8.1 daemon
#   anyway — i.e. the pin is silently ignored.
#
# The container builds soldr from the local checkout (the build context
# must be the soldr repo root), stages three fake zccache binaries under
# /pinned-bin/, pins them with `soldr update-zccache`, runs a tiny
# `soldr cargo build`, and inspects the `soldr: zccache source: ...`
# diagnostic that PR #421 promised would name the actual source. If
# that line says `managed` instead of `pinned`, the harness exits
# non-zero with a verbose dump.
#
# Build (from the soldr repo root):
#   docker build -t soldr-pin-repro -f bench/docker/update-zccache-pin-honored/Dockerfile .
# Run:
#   docker run --rm soldr-pin-repro
#
# The companion `run.sh` does both in one shot.

# Single-stage on purpose: the repro needs cargo at runtime because the
# bug only triggers during `soldr cargo build`. Multi-stage with a
# slim runtime would force us to also install a rust toolchain there.
FROM rust:1.95.0-alpine

# bash + jq for the repro script; coreutils so `comm` / `sort` / `sha256sum`
# behave like GNU coreutils (busybox versions differ in some flags); git
# because cargo emits a deprecation warning without it on some setups.
RUN apk add --no-cache musl-dev bash jq coreutils git

WORKDIR /soldr

# Copy only what's needed to build soldr-cli. The root `.dockerignore`
# already excludes target/, .git/, .venv/, etc., so a blanket COPY is
# acceptable, but being explicit keeps the build layer-cacheable across
# unrelated file changes (e.g. README edits don't bust the cargo layer).
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

# Build the soldr binary against the Alpine musl toolchain. The
# resulting binary is statically linked and lands at
# /soldr/target/release/soldr.
RUN cargo build --release -p soldr-cli \
 && install -m 0755 target/release/soldr /usr/local/bin/soldr \
 && cargo clean

COPY bench/docker/update-zccache-pin-honored/repro.sh /repro.sh
RUN chmod +x /repro.sh

# Run in /work because soldr cargo build needs a writable Cargo project
# dir; /soldr is left in case anyone wants to inspect the source.
WORKDIR /work

ENTRYPOINT ["/repro.sh"]
