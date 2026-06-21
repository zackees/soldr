# Docker: Linux → Windows x86_64 cross-compile demo

End-to-end proof that a Rust binary cross-compiled inside a **Linux**
Docker container runs cleanly on a **Windows x86_64** host — no Windows
build environment required on the developer / CI machine.

## What this directory contains

| | |
|---|---|
| `Dockerfile` | `messense/cargo-zigbuild` base + the `x86_64-pc-windows-gnu` rustup target |
| `crate/` | Tiny Rust source: prints a recognizable signature and exits 0 |
| `build.sh` | One-command orchestrator: build image, cross-compile, copy `.exe` out, smoke-test |
| `out/` | Host-side landing zone for the cross-compiled `.exe` (gitignored) |

## One-command reproducer

From a host with Docker installed:

```sh
./examples/docker-cross-win/build.sh
```

The script:

1. `docker build` the image (cached after first run).
2. Bind-mounts `crate/` into the container, runs `cargo zigbuild --target x86_64-pc-windows-gnu --release`.
3. Copies the produced `docker-cross-win-demo.exe` to `out/`.
4. Prints the binary size, file type, and sha256.
5. If the host can run a Windows PE (native Windows, MSYS bash, or `wine`-on-linux), runs the `.exe` and asserts it prints `docker-cross-win-demo OK` with `target_os = windows`.

On a plain linux host without `wine`, step 5 is skipped with a friendly note — but the artifact is still produced and verified at the file level. Pass `--no-host-check` to skip the run explicitly (useful for CI lanes that only build).

## Why `cargo-zigbuild` rather than mingw or `cross`

`cargo-zigbuild` uses `zig` as the cross-linker. That eliminates the
two pain points the alternatives carry:

- **MinGW toolchain on the host**: Installing the right `mingw-w64`
  multi-architecture package and threading model is fiddly and
  distro-specific. With zig the linker is one tar.xz that's already
  baked into the `messense/cargo-zigbuild` image.
- **`cross`'s QEMU-in-Docker overhead**: `cross` ships per-target
  containers that run via QEMU emulation for some workflows. zig
  compiles `target/` natively on the host arch and only swaps the
  linker, so build times match a native compile.

The same toolset is what `zackees/setup-soldr`'s `cross-bootstrap.ts`
uses on CI (linux → windows-gnu, linux → linux-musl, and as of
[setup-soldr#385](https://github.com/zackees/setup-soldr/pull/385)
linux → apple-darwin). This example is the standalone Docker analogue
of that CI lane.

## What it does NOT cover

- **`x86_64-pc-windows-msvc`**: cargo-zigbuild can do MSVC via
  `--target ... --enable-msvc` if `cargo-xwin` is also installed, but
  the demo intentionally sticks with `*-windows-gnu` — the GNU target
  is fully redistributable, doesn't need Microsoft licensing, and the
  produced `.exe` runs on any Windows install without runtime DLLs.
- **`i686-pc-windows-gnu` (32-bit Windows x86)**: same recipe works
  with `rustup target add i686-pc-windows-gnu` plus
  `--target i686-pc-windows-gnu` — left out of the default demo to
  keep the layer count down.

## Pinning notes

`messense/cargo-zigbuild:0.20.0` pins all three of rust, zig, and
cargo-zigbuild together. Bumping the tag is the only knob — there is
no separate `RUSTUP_VERSION` or `ZIG_VERSION` to keep in sync.
