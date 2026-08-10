# syntax=docker/dockerfile:1.7
#
# Iterative Docker Linux dev/test runner for the running-process workspace.
#
# Why this image exists: broker v1 work (#464, #466) is developed inside
# this container so the Windows host's toolchain / cargo state is never
# touched. Named volumes (NOT host bind mounts) hold target/, CARGO_HOME,
# RUSTUP_HOME, the uv cache, and soldr per-user state so cargo's mtime
# fingerprint survives container restarts. Without that, Windows + Docker
# Desktop's WSL2 9P translation rewrites mtimes on every container start
# and cargo rebuilds the world (~minutes on this workspace vs ~seconds
# when the volumes are warm).
#
# Coexists with the CI-artifact Dockerfiles at repo root
# (Dockerfile.linux-build, .linux-lint, .linux-pytest); none of those are
# touched by this image. Driver: ci/dev_docker.py.

FROM rust:1.94.1-bookworm

# Build deps:
#   build-essential / pkg-config / libssl-dev — for any cc-rs / -sys crates
#   protobuf-compiler — proto/daemon.proto is compiled by build.rs
#   python3 / python3-pip / python3-venv — uv runs the repo's Python flows
#   git / curl / ca-certificates — uv install, cargo registry
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        gdb \
        git \
        libssl-dev \
        pkg-config \
        protobuf-compiler \
        python3 \
        python3-dev \
        python3-pip \
        python3-venv \
        zstd \
 && rm -rf /var/lib/apt/lists/*

# gdb is for the test-watchdog (crates/test-watchdog) to capture all-thread
# backtraces when a Rust integration test hangs — see CLAUDE.md "Per-test
# deadlock guard". Without gdb, watchdog kills the test with no diagnostics.

# uv — the repo's Python entrypoints (./test, ./lint, build.py) shell out
# to `uv run`. Install to /usr/local/bin so it survives any CARGO_HOME
# volume mount.
RUN curl -LsSf https://astral.sh/uv/install.sh | env UV_INSTALL_DIR=/usr/local/bin sh

# Pin the toolchain at image-build time, matching rust-toolchain.toml at
# the repo root (channel 1.94.1, components clippy + rustfmt). The image's
# CARGO_HOME=/usr/local/cargo and RUSTUP_HOME=/usr/local/rustup are mounted
# by named volumes at run time; Docker initializes empty volumes with the
# image content on first mount so the pre-installed toolchain survives.
RUN rustup default 1.94.1 \
 && rustup component add clippy rustfmt

# soldr is INTENTIONALLY NOT installed inside this image.
#
# The host's .claude/settings.json force_soldr.py PreToolUse hook enforces
# soldr-wrapping for cargo commands on the HOST. That hook does not run
# inside the container — it only sees the `docker run ...` invocation from
# the host. Inside the container, direct cargo is the natural path.
#
# soldr 0.7.55's zccache integration also breaks reliably inside fresh
# containers with a "private daemon cache dir mismatch" on session-start;
# until that's tracked down upstream, direct cargo via the named
# CARGO_TARGET_DIR volume gives us the caching benefit we actually need
# (mtime fingerprint survival across container restarts) without the
# layered failure mode.
#
# The Python entrypoints (ci.install / ci.test / ci.lint / build.py) all
# detect `shutil.which("soldr")` and gracefully fall through to direct
# cargo when soldr is absent — verified in ci/install.py and observed in
# the build wheel / test / lint paths.

# Set the dev paths explicitly so future base-image migrations don't
# silently change them. CARGO_TARGET_DIR is set AFTER cargo install above
# so the soldr build doesn't leak artifacts into the image at /work/target.
ENV CARGO_TARGET_DIR=/work/target

# uv writes its venv into the project directory by default; redirect it
# so .venv doesn't collide with the host's .venv via the bind-mounted
# source (Linux ELF binaries appearing inside a Windows checkout would
# confuse host tooling and host-side `uv sync`).
ENV UV_PROJECT_ENVIRONMENT=/uv/venv
ENV UV_CACHE_DIR=/uv/cache

# PYTHONUNBUFFERED matches the repo convention from CLAUDE.md.
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1

# Disable the aggressive lint idle-timeout (ci/lint.py default 10s). Clippy
# on this workspace routinely has >10s output gaps mid-compile, especially
# on a cold cache. Empty value tells ci/lint.py to run unlimited. Host
# behavior is unaffected; this is container-scope only.
ENV RUNNING_PROCESS_LINT_COMMAND_TIMEOUT_SECONDS=""

# Vanilla Debian containers ship without /etc/machine-id (the file is
# normally seeded by systemd / dbus on a real system). The broker's user
# identity derivation and the doctor diagnostic both read it; without
# /etc/machine-id, the tests at crates/running-process/tests/broker/
# doctor.rs report a `platform:path-budget` failure and several adjacent
# tests fail-fast. Synthesize a stable 128-bit hex value at image-build
# time — not used for real identity, just a non-empty value.
RUN cat /proc/sys/kernel/random/uuid | tr -d '-' > /etc/machine-id

# XDG_RUNTIME_DIR is normally set per-session by systemd-logind / pam-systemd
# at login. The broker derives its socket / runtime dir from this when
# present and falls back to platform defaults otherwise; the fallback
# trips a `sockets:runtime-dir` doctor failure on container roots. Provide
# a real per-user dir at image build time. Tests treat this as a
# container-compatible runtime root.
ENV XDG_RUNTIME_DIR=/run/user/0
RUN mkdir -p /run/user/0 && chmod 700 /run/user/0

# Source is mounted by the orchestrator at /work; no COPY of source here
# so each `docker run` reuses the cached image but operates on the live
# worktree.
WORKDIR /work

# Default: drop into bash. ci/dev_docker.py overrides with explicit args.
CMD ["bash"]
