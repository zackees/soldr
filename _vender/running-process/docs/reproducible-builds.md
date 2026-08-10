# Reproducible builds

`RUNNING_PROCESS_REPRODUCIBLE=1` is an opt-in build seam (#392) that
normalizes the sources of non-determinism in our build so two builds of
the same commit produce byte-identical artifacts. Default builds are
completely unchanged — the seam is a no-op unless the variable is set.

## What the seam does

Implemented in [`ci/reproducible.py`](../ci/reproducible.py) and applied
automatically by `ci.env.build_env()` (so `uv run build.py` picks it up
for both the maturin wheel build and the bundled `daemon-trampoline`
binary):

| Lever | Effect |
| --- | --- |
| `SOURCE_DATE_EPOCH` = HEAD commit time (`git log -1 --format=%ct`, clamped to 1980-01-01 for zip compatibility) | maturin stamps wheel zip entries with this instead of wall-clock mtimes |
| `RUSTFLAGS += --remap-path-prefix` for the workspace root, `CARGO_HOME`, and `RUSTUP_HOME` | strips host-specific absolute paths from debuginfo and panic messages (`/running-process/src`, `/running-process/cargo`, `/running-process/rustup` tokens) |
| `CARGO_INCREMENTAL=0` | incremental compilation artifacts are not stable across runs |
| `RUSTFLAGS += -Clink-arg=/Brepro` (Windows only) | MSVC `link.exe` otherwise embeds a wall-clock timestamp and a timestamp-seeded signature in the PE debug directory; `/Brepro` switches both to content-derived hashes |

A pre-set `SOURCE_DATE_EPOCH` in the environment is respected (not
overwritten), so release pipelines can pin their own epoch.

There is no build-time stamping in this repo (`build.rs` only runs prost
codegen from checked-in `.proto` files, with no timestamps or host
identifiers), and `Cargo.lock` is committed, so dependency versions are
already stable.

## Verification recipe

Build twice, compare SHA256. The repo ships a one-shot verifier that
builds the debug-profile `runpm` binary twice — running
`cargo clean -p running-process` in between so the workspace crate is
fully recompiled while cached dependency artifacts are reused — and
compares digests:

```bash
RUNNING_PROCESS_REPRODUCIBLE=1 uv run --no-project --module ci.reproducible --verify
```

(The verifier forces the seam on itself, so the env var prefix is
optional.) Exit code 0 means the two builds were byte-identical; the
two SHA256 digests are printed either way.

To verify the wheel instead (slower):

```bash
export RUNNING_PROCESS_REPRODUCIBLE=1
uv run build.py --release && sha256sum dist/running_process-*.whl
# clean dist/ and the workspace crates, then repeat and compare
```

## CI spot-check

The `reproducible-spot-check` job in
[`.github/workflows/linux-x86-build.yml`](../.github/workflows/linux-x86-build.yml)
runs the `runpm` double-build verifier on every push/PR. The check
deliberately targets the small debug `runpm` binary rather than the
full wheel to keep the job cheap (a wheel double-build would roughly
double the longest CI job); the same seam covers the wheel path.

## Platform notes

- **Linux is the acceptance platform.** The CI spot-check runs on
  `ubuntu-24.04`.
- **Windows**: with the seam's `/Brepro` link flag the `runpm`
  double-build is byte-identical locally (verified on
  `x86_64-pc-windows-msvc`, rustc 1.94.1). Note the comparison covers
  the `.exe` only — the side-band `.pdb` file is not hash-checked, and
  the embedded PDB path is the same on a single machine but differs
  across machines with different checkout paths (`/PDBALTPATH` is the
  lever if cross-machine Windows reproducibility is ever needed).
