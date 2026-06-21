# Docker: Linux → Windows cross-compile demo

End-to-end proof that a Rust binary cross-compiled inside a **Linux**
Docker container runs cleanly on a **Windows** host — no Windows build
environment required on the developer / CI machine. The example covers
four Windows targets from a single image:

| Target triple | ABI | Cross-link tool | Notes |
|---|---|---|---|
| **`x86_64-pc-windows-msvc`** | **MSVC** | `cargo-xwin` | **Default.** The canonical Microsoft ABI for Windows desktop x64. Matches soldr's "MSVC on Windows always" design rule. |
| `aarch64-pc-windows-msvc` | MSVC | `cargo-xwin` | Official MSVC ABI for Windows ARM64. |
| `x86_64-pc-windows-gnu` | GNU | `cargo-zigbuild` | Traditional GNU; useful when MSVC licensing is a concern. |
| `aarch64-pc-windows-gnullvm` | gnullvm | `cargo-zigbuild` | Alternate Windows ARM64 ABI, free with the zigbuild image (no MSVC SDK). |

## What this directory contains

| | |
|---|---|
| `Dockerfile` | `messense/cargo-zigbuild` base + 4 rustup targets + `cargo-xwin` for MSVC |
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

# Windows x64, GNU ABI (no MSVC SDK download)
./examples/docker-cross-win/build.sh --target x86_64-pc-windows-gnu

# Windows ARM64, gnullvm ABI (no MSVC SDK download)
./examples/docker-cross-win/build.sh --target aarch64-pc-windows-gnullvm
```

The script:

1. `docker build` the image (cached after first run; first build pulls ~700 MB of base layers + `cargo install cargo-xwin`).
2. Bind-mounts `crate/` into the container, dispatches to `cargo zigbuild` or `cargo xwin build` based on the target ABI.
3. Copies the produced `docker-cross-win-demo.exe` to `out/<triple>/`.
4. Prints binary size, `file(1)` type, and sha256.
5. **Arch sanity check**: asserts the produced PE's architecture string matches the requested target. A misconfigured Dockerfile or stale build cache could otherwise silently ship the wrong architecture.
6. **Host-side smoke test** (when possible): if the host can run a Windows PE *and* the PE's arch matches the host's arch, executes the `.exe` and asserts it prints `docker-cross-win-demo OK` with `target_os = windows`.

When the host arch and PE arch don't match (e.g. running an ARM64 target on an x86_64 Windows host), the run check is correctly skipped with a friendly note — the cross-compile is still proven by the PE-header verification.

Pass `--no-host-check` to skip the run unconditionally (useful for CI lanes that only build).

## MSVC vs gnullvm for Windows ARM64

Two ABIs exist for `aarch64-pc-windows`:

- **`aarch64-pc-windows-msvc`** — the mainstream Microsoft ABI. Native Windows tooling, Visual Studio integration, and most third-party DLLs use this ABI. **This is what you want when interoperating with the rest of the Windows ecosystem.**
- **`aarch64-pc-windows-gnullvm`** — same `gnullvm` family the x86_64 GNU target uses, but Windows ARM64. Free with the zigbuild image (no MSVC SDK download), but ABI-incompatible with MSVC import libraries. **Fine for pure-Rust binaries with no FFI into MSVC DLLs.**

The two binaries have different PE section layouts (msvc 7 sections, gnullvm 6 in this demo) and different import-library shapes. End users of pure-Rust applications won't notice the difference.

This example demonstrates both so the reader can pick.

## Why `cargo-zigbuild` and `cargo-xwin` instead of mingw or `cross`

Both `cargo-zigbuild` and `cargo-xwin` are by the same author (messense) and avoid the two pain points the alternatives carry:

- **No MinGW toolchain on the host**: zig (or clang-cl + lld-link from xwin) handles PE/COFF emission. No distro-specific `mingw-w64` packaging headaches.
- **No `cross`-style QEMU-in-Docker overhead**: both tools compile `target/` natively on the host arch and only swap the linker, so build times match a native compile.

The same toolset is what `zackees/setup-soldr`'s `cross-bootstrap.ts` uses on CI (linux → windows-gnu, linux → linux-musl, and as of [setup-soldr#385](https://github.com/zackees/setup-soldr/pull/385) linux → apple-darwin). This example is the standalone Docker analogue of that CI lane.

## What it does NOT cover

- **`i686-pc-windows-{gnu,gnullvm,msvc}` (32-bit Windows x86)**: same recipe works with the matching `rustup target add`. The script's PE-arch check already knows the right `file(1)` label (`Intel 80386`).

## Pinning notes

- `messense/cargo-zigbuild:0.20.0` pins rust (1.85), zig, and cargo-zigbuild together. Bumping the tag is the only knob.
- `cargo-xwin@0.19.2` is pinned because newer versions require rust 1.89+ (the base image ships 1.85). If you bump the cargo-zigbuild tag to one that ships rust 1.89+, also bump cargo-xwin's version pin in the Dockerfile.
- The MSVC SDK is downloaded on first `cargo xwin build` invocation (~700 MB) and cached in `.xwin-cache/` on the host. The cache is gitignored and shared across runs.
