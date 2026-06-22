# Docker: one-image cross-compile for all 8 soldr targets

End-to-end proof that a single **linux x86 docker image** — with `soldr
prepare --target all` baked in at build time — can cross-compile a Rust
binary for every triple in `[workspace.metadata.soldr].targets`:

| Family | Triples |
|---|---|
| Windows MSVC | `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc` |
| macOS | `x86_64-apple-darwin`, `aarch64-apple-darwin` |
| Linux glibc | `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` |
| Linux musl | `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` |

No Windows runner, no macOS runner, no per-target setup/teardown — one
image, eight `docker run`s, eight artifacts. Replaces the per-OS GitHub
Actions matrix with a self-contained local recipe.

## What's in the image

The Dockerfile bakes three layers worth of cross-compile state into
one image, so a fresh `docker run` does **zero network fetches** and
goes straight to `cargo build`:

1. **soldr** — built from source against `SOLDR_GIT_REF` (default `main`) in a stage-1 builder, then `COPY --from=soldr-builder` into the runtime image. Avoids the PyPI release cadence — the example rides latest main.
2. **rustup + pinned channel** — `soldr bootstrap` then `soldr
   toolchain install` against the included `rust-toolchain.toml`.
3. **Every cross-compile asset for every target** — `soldr prepare
   --target <comma-separated-list>` with the 8 triples hard-coded into
   the Dockerfile. The comma-separated form is used because at image-
   build time no source volume is mounted, so `--target all` (which
   reads `[workspace.metadata.soldr].targets` from a `Cargo.toml`)
   can't see the workspace yet. This pulls zig, LLVM, the Apple SDK,
   the xwin MSVC CRT cache, and runs `rustup target add` for all 8
   triples. Requires soldr [PR #925](https://github.com/zackees/soldr/pull/925).

The resulting image is ~1.5 GiB but every subsequent `docker run`
arrives warm.

## One-command reproducer

From the repo root, with Docker installed:

```sh
# Build all 8 targets
./examples/docker-cross-all/build.sh

# Single target
./examples/docker-cross-all/build.sh --target x86_64-pc-windows-msvc

# Reuse an existing image (skip the docker build step)
./examples/docker-cross-all/build.sh --no-build-image
```

The script:

1. `docker build` the image if needed (cached after first run).
2. For each requested target: `docker run` the image, executing
   `soldr cargo build --release --target <triple>` against the bind-
   mounted `crate/` directory.
3. Copies each produced binary to `out/<triple>/`.
4. Prints per-target wall-clock, binary size, file type, and `du -sh`
   of `target/<triple>/` and `target/<triple>/release/incremental/`.
5. Ends with a final `target/` partition report so the cargo per-
   platform layout is visible at a glance.

## Why this is the same code path setup-soldr exercises on CI

The bake step is plain `soldr prepare --target all` — the exact entry
point the upstream `setup-soldr` GitHub Action calls. Building this
image and running the cross-compile inside it is equivalent to the
matrix the CI lanes exercise, with one runner instead of N.

## Layout

| | |
|---|---|
| `Dockerfile` | Debian slim + soldr + rustup + `soldr prepare --target <list>` |
| `rust-toolchain.toml` | Channel pin used by `soldr toolchain install` inside the image |
| `crate/` | Tiny Rust source — prints `OK target_os=… target_arch=…` |
| `build.sh` | Host orchestrator — builds image, loops targets, reports sizes |
| `out/<target>/` | Host-side landing zone per triple (gitignored) |

## What's next

Iteration follow-ups tracked outside this PR:

- **Cache save/restore** — call `soldr save <stable-path>` after the
  bake + build, `soldr load <same-path>` on the next run. Goal: one
  cache artifact per repo, stable name, no per-target fan-out.
- **Optimization pass** — lld linker, codegen-units, parallel target
  builds, identify and eliminate redundant prepare-time downloads.
- **Logging** — surface zccache hit rate per triple so the per-target
  `target/` partition story is auditable from one report.

Refs zackees/soldr#914 (the `--target all` flag this recipe exercises).
