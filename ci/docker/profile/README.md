# soldr Linux profile harness

Captures on-CPU + off-CPU flame charts for **soldr's full cargo build
pipeline** — argv parsing → cargo front door → wrapper exec → zccache
shellout. Three scenarios per run let you triangulate where the time
goes:

| Scenario | What runs | What it measures |
|---|---|---|
| `cold` | `soldr cargo build` against a freshly-extracted fixture with empty cache + empty target | Worst case — soldr's full dispatch overhead is in frame |
| `warm` | Same fixture, **primed cache + cleared target**, then sampled on the second build | Steady-state cache-hit cost. Should approach bare cargo's incremental overhead. |
| `bare` | `cargo build` directly, no soldr, no zccache | Theoretical-max anchor. `cold - bare` and `warm - bare` give honest soldr-overhead numbers. |

## How to run

```bash
# From the soldr repo root.
docker build -f ci/docker/profile/Dockerfile.perf-linux -t soldr-profile-linux .

# UTC stamp keeps multiple runs separate.
STAMP=$(date -u +%Y-%m-%d-%H%M)
OUT="$(pwd)/.codex-artifacts/soldr-wider-perf-${STAMP}"
mkdir -p "${OUT}"

docker run --rm --privileged \
    --cap-add=SYS_ADMIN --cap-add=SYS_PTRACE \
    -v "$(pwd)":/work \
    -v "${OUT}":/out \
    soldr-profile-linux
```

`--privileged` + the two capabilities unblock `perf_event_open` and
`BPF_PROG_LOAD` inside the container. WSL2's kernel can still refuse
bpftrace stack-id lookups; the harness falls back to `perf record
sched:sched_switch` for the off-CPU pass when bpftrace produces no
data.

## What we changed and why

The perf fixtures (`perf/fixtures/medium/Cargo.toml` and
`perf/fixtures/sqlite-link/Cargo.toml`) now pin
`[profile.dev] debug = false` and `incremental = false`. soldr's
managed-cargo path already forces `debug = false` on the cold
scenario, so before this change bare cargo was silently running with
`debug = true` (the dev-profile default) and paying for debuginfo
codegen the cold scenario never did. That inflated bare's wall-clock
and made the `cold - bare` gap (24.6 s in the 2026-06-26-1534 bundle)
a *lower bound* on real soldr overhead rather than an honest
measurement. Pinning the profile in the fixture itself brings cold /
warm / bare to like-for-like settings so future profile bundles
measure soldr's dispatch overhead instead of debuginfo asymmetry.
See L8 in soldr#980.

## Output layout

```
<OUT>/
├── run.log                      # top-level driver log
├── build.log                    # soldr cargo build (the soldr-cli build, not the workload)
├── cold/
│   ├── perf.data                # raw on-CPU samples
│   ├── perf-sched.data          # raw sched_switch samples
│   ├── oncpu.folded             # stackcollapse-perf.pl output
│   ├── oncpu.svg                # Brendan Gregg flame chart
│   ├── offcpu.folded            # bpftrace OR perf-sched fallback
│   ├── offcpu.svg               # same renderer, --color=io
│   ├── workload.stdout.log
│   ├── workload.stderr.log
│   ├── perf.log
│   └── perf-sched.log
├── warm/  ... same shape ...
└── bare/  ... same shape ...
```

## Tunables

| Env var | Default | Effect |
|---|---|---|
| `SOLDR_PROFILE_HZ` | `99` | `perf record -F` rate. Higher = finer-grained but bigger `perf.data` |
| `SOLDR_PROFILE_SCENARIOS` | `cold warm bare` | Space-separated scenario list. Drop tokens to skip cases. |
| `SOLDR_PROFILE_FIXTURE` | `/work/perf/fixtures/medium.tar.gz` | Tarball that contains the cargo workspace to profile against |
| `SOLDR_PROFILE_FIXTURE_DIR` | `medium` | Subdirectory inside the tarball that contains the `Cargo.toml` |

The fixture defaults to soldr's `perf/fixtures/medium` (the same one
the Perf Matrix uses), so the profile data lines up with the
gate-workflow numbers.

## Why three runs

- **`cold - bare`** = soldr's worst-case overhead on a fresh checkout.
  This number is the headline cost a new contributor pays.
- **`warm - bare`** = soldr's steady-state overhead. Should be small.
  If it isn't, the daemon / wrapper / IPC chain is doing real work
  when it shouldn't be.
- **`cold - warm`** = the value soldr delivers — the gap that
  caching closes. The bigger this is, the more justified the
  per-build overhead is.

The five-subagent analysis pass that consumes this bundle uses all
three numbers; the gap analysis specifically anchors against `bare`.

## Comparison to zccache's profile harness

This rig is a direct adaptation of `zccache/ci/docker/profile/`.
Differences:

- **Workload**: `soldr cargo build` instead of `cargo test perf_rustc_zccache_vs_sccache`.
- **Three scenarios per run** instead of one; the existing zccache
  rig was designed for diffing across zccache PR landings, not for
  diffing across scenarios within one run.
- **Same toolchain pin, same samplers, same SVG style** so the two
  bundles are directly comparable side-by-side.

The intent is that an operator can run both rigs and overlay the
results — soldr's flame chart shows the dispatch chain; zccache's
shows the compile-cache hot path; together they explain the full
wall-clock cost of a `soldr cargo build`.
