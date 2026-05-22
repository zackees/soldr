# `soldr update-zccache` pin-honored Docker repro

Self-contained Docker harness for [zackees/soldr#424][issue]:
`soldr update-zccache <path>` registers a successful path-pin, but a
subsequent `soldr cargo build` spawns the managed v1.8.1 daemon anyway —
the pin is silently ignored.

This harness builds soldr from the local checkout, stages three fake
zccache binaries under `/pinned-bin/`, pins them, runs a tiny
`soldr cargo build`, and inspects the **`soldr: zccache source: ...`**
diagnostic that [PR #421][pr-421] added precisely to make the
pin-vs-managed routing observable.

If the diagnostic says `managed` instead of `pinned`, the bug from
#424 reproduces and the harness exits non-zero with a dump of the pin
status JSON, the contents of `~/.soldr/bin/`, and the last 80 lines of
`cargo build` stderr.

## Run it

From the soldr repo root:

```bash
bash bench/docker/update-zccache-pin-honored/run.sh
```

Or by hand:

```bash
docker build -t soldr-pin-repro \
  -f bench/docker/update-zccache-pin-honored/Dockerfile .
docker run --rm soldr-pin-repro
```

Interactive debug (drops into a shell inside the built image, skipping
the repro):

```bash
bash bench/docker/update-zccache-pin-honored/run.sh -- -it --entrypoint /bin/bash
```

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | Pin honored. `soldr: zccache source: pinned (...)` was emitted. |
| 1    | **Bug reproduced.** Pin was registered but `soldr cargo build` resolved to `managed (...)`. |
| 2    | Setup failure — `update-zccache` itself errored, or the pin status JSON didn't reflect the requested path pin. |

## Current status

Building this harness against soldr `main` HEAD (post-#421) **passes** —
the diagnostic emits `soldr: zccache source: pinned (...)` as it
should. So either:

1. **#421 actually fixed the routing bug** for this exact code path, and
   the residual failures the issue reports come from somewhere else
   (real zccache binary behavior, fingerprinting, pre-existing managed
   installs the harness doesn't simulate, etc.), **or**
2. **the bug only triggers under conditions this synthetic repro
   doesn't reach** — e.g. with real signed v1.8.1 binaries, with a
   pre-existing `~/.soldr/bin/zccache-1.8.1/` install, or with a
   particular `RUSTC_WRAPPER` / `--zccache` flag combination.

Either way, this harness now serves as a **regression test**: if a
future change re-breaks the path-pin spawn path, the diagnostic flips
to `managed (...)` and this exits 1 with a full diagnostic dump. Add
or extend variants here when you discover the conditions that surface
the live bug.

## What's inside

| File | Role |
|------|------|
| `Dockerfile` | Single-stage `rust:1.94.1-alpine` image. Copies `Cargo.toml`/`Cargo.lock`/`rust-toolchain.toml` + `crates/` from the build context (the repo root), builds `soldr-cli --release`, installs the binary to `/usr/local/bin/soldr`, and `cargo clean`s to shrink the image. |
| `repro.sh` | In-container script. Stages fake binaries, registers the pin, asserts pin status, runs `soldr cargo build`, grep's stderr for the source diagnostic, and prints a verdict. |
| `run.sh` | Host-side convenience wrapper — `docker build` + `docker run` in one step. Forwards anything after `--` to `docker run`. |
| `README.md` | This file. |

## Why this can't be a pure Rust integration test

The bug only manifests when soldr resolves zccache at runtime against a
real `$HOME/.soldr/bin/` layout. A unit test would have to mock the
entire fetch + pin + spawn pipeline, at which point you're not testing
the bug anymore. Docker gives us a clean `$HOME` and a writable
`~/.soldr/` per run, with zero pollution of the host's soldr cache.

The harness deliberately does **not** ship with the rest of the test
suite — it requires Docker, takes a couple of minutes for the cargo
build inside the container, and is a regression repro, not a
day-to-day smoke test. Run it locally when investigating #424 or any
follow-up to #420 / #421.

[issue]: https://github.com/zackees/soldr/issues/424
[pr-421]: https://github.com/zackees/soldr/pull/421
