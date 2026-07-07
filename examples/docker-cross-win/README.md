# Docker: Linux → Windows MSVC cross-compile demo

End-to-end proof that a Rust binary cross-compiled inside a **Linux**
Docker container runs cleanly on a **Windows** host through the MSVC ABI.
This is a legacy MSVC/cargo-xwin example; it is intentionally separate from
the blessed Windows GNU path.

| Target triple | ABI | Cross-link tool | Notes |
|---|---|---|---|
| **`x86_64-pc-windows-msvc`** | **MSVC** | `cargo-xwin` | **Default.** The canonical Microsoft ABI for Windows desktop x64. Matches soldr's "MSVC on Windows always" design rule. |
| `aarch64-pc-windows-msvc` | MSVC | `cargo-xwin` | Official MSVC ABI for Windows ARM64. |

For `x86_64-pc-windows-gnu`, use `soldr prepare --target
x86_64-pc-windows-gnu` and `soldr build --target x86_64-pc-windows-gnu`
on a Windows x64 host. Do not use this Docker example or cargo-zigbuild as
the Windows GNU path.

## What this directory contains

| | |
|---|---|
| `Dockerfile` | Rust base image + two MSVC rustup targets + `cargo-xwin` |
| `crate/` | Tiny Rust source: prints a recognizable signature and exits 0 |
| `build.sh` | One-command orchestrator: build image, cross-compile, copy `.exe` out, verify PE arch, smoke-test |
| `out/<target>/` | Host-side landing zone for the cross-compiled `.exe` (gitignored) |
| `.xwin-cache/` | xwin sysroot download cache (~700 MB, gitignored; first MSVC build downloads, subsequent ones are offline) |

## One-command reproducer

From a host with Docker installed:

```sh
# Default: Windows x64, MSVC ABI (the canonical one)
./examples/docker-cross-win/build.sh

# Windows ARM64, MSVC ABI
./examples/docker-cross-win/build.sh --target aarch64-pc-windows-msvc

```

The script:

1. `docker build` the image (cached after first run; first build pulls the Rust base image + `cargo install cargo-xwin`).
2. Bind-mounts `crate/` into the container and dispatches to `cargo xwin build`.
3. Copies the produced `docker-cross-win-demo.exe` to `out/<triple>/`.
4. Prints binary size, `file(1)` type, and sha256.
5. **Arch sanity check**: asserts the produced PE's architecture string matches the requested target. A misconfigured Dockerfile or stale build cache could otherwise silently ship the wrong architecture.
6. **Host-side smoke test** (when possible): if the host can run a Windows PE *and* the PE's arch matches the host's arch, executes the `.exe` and asserts it prints `docker-cross-win-demo OK` with `target_os = windows`.

When the host arch and PE arch don't match (e.g. running an ARM64 target on an x86_64 Windows host), the run check is correctly skipped with a friendly note — the cross-compile is still proven by the PE-header verification.

Pass `--no-host-check` to skip the run unconditionally (useful for CI lanes that only build).

## Windows GNU

Windows GNU is no longer demonstrated through a Linux Docker/cargo-zigbuild
lane. The blessed path is the managed MinGW-w64 GCC bundle plus GNU-shaped
syslibs documented in `docs/CROSS_COMPILE.md`.

## What it does NOT cover

- **Windows GNU / gnullvm**: use the managed MinGW path in soldr, not this
  Docker example.
- **`i686-pc-windows-msvc` (32-bit Windows x86)**: same recipe can be
  extended with the matching `rustup target add` and PE-arch check.

## Pinning notes

- `rust:1.85-bookworm` pins the Rust version used by the Docker example.
- `cargo-xwin@0.19.2` is pinned because newer versions require rust 1.89+. If you bump the Rust base image to 1.89+, also recompute the cargo-xwin pin.
- The MSVC SDK is downloaded on first `cargo xwin build` invocation (~700 MB) and cached in `.xwin-cache/` on the host. The cache is gitignored and shared across runs.
