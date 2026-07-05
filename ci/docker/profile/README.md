# soldr Linux profile harness

Runs the **perf-matrix cycle** (`PERF.md`) inside a Linux Docker
container under simultaneous on-CPU (linux-perf) and off-CPU
(bpftrace / perf-sched fallback) samplers, folds every scenario's
stacks with FlameGraph, and produces a **top-5 slowest items**
markdown table plus per-scenario SVG flame graphs.

## What runs

The four authoritative perf-matrix scenarios against `perf/fixtures/medium`:

| Scenario | What it measures |
|---|---|
| `cold-tar-untar-warm` | cold build → tar compress → untar into fresh cache → warm build (gates soldr save/load fidelity) |
| `worktree-share` | primary checkout build → `git worktree add` second-checkout build sharing the cache (gates `ZCCACHE_PATH_REMAP=auto`) |
| `touch-no-change` | cold build → touch every source file (fresh mtimes, same content) → cargo clean → rebuild (gates content-hash freshness) |
| `build-then-check` | `cargo build --release` → `cargo check` on the same sources (gates cross-verb cache reuse) |

Each scenario is invoked as `bash perf/scenarios/<name>/run.sh <fixture-dir>`
inside the container, wrapped by `scripts/run_scenario.sh`.

Tokio-events profiling is deliberately out of scope for this iteration;
soldr-daemon has zero `console-subscriber` wiring today. Adding it is a
separate follow-up (mirror the pattern in `_vender/zccache/crates/zccache/src/bin/zccache-daemon.rs`).

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

## Output layout

```
<OUT>/
├── run.log                      # top-level driver log
├── build.log                    # soldr-cli release build (for the SOLDR_BIN)
├── top5.md                      # aggregated top-5 slowest items across scenarios
├── cold-tar-untar-warm/
│   ├── scenario.log             # per-scenario driver log
│   ├── perf.data                # raw on-CPU samples
│   ├── perf-sched.data          # raw sched_switch samples
│   ├── oncpu.folded             # stackcollapse-perf.pl output
│   ├── oncpu.svg                # Brendan Gregg flame graph
│   ├── offcpu.folded            # bpftrace OR perf-sched fallback
│   ├── offcpu.svg               # same renderer, --color=io
│   ├── scenario.stdout.log
│   ├── scenario.stderr.log
│   ├── perf.log
│   ├── perf-sched.log
│   └── offcpu.bpftrace.log
├── worktree-share/ ... same shape ...
├── touch-no-change/ ... same shape ...
└── build-then-check/ ... same shape ...
```

`top5.md` renders as:

```
| Rank | Function | On-CPU samples | Off-CPU samples | Total | Dominant scenario |
|------|----------|----------------|-----------------|-------|-------------------|
| 1    | `rustc_middle::ty::TyCtxt::lookup` | 12,401 | 340 | 12,741 | build-then-check |
| ...  | ...                                | ...    | ... | ...    | ...              |
```

Leaf frames are normalized (address offsets stripped so `foo+0x12` and
`foo+0x40` bucket together) before ranking.

## Tunables

| Env var | Default | Effect |
|---|---|---|
| `SOLDR_PROFILE_HZ` | `99` | `perf record -F` rate. Higher = finer-grained but bigger `perf.data` |
| `SOLDR_PROFILE_SCENARIOS` | `cold-tar-untar-warm worktree-share touch-no-change build-then-check` | Space-separated scenario list. Drop tokens to skip cases. |
| `SOLDR_PROFILE_FIXTURE_NAME` | `medium` | Fixture name (extracted via `perf/lib/extract.sh`) |
| `SOLDR_PROFILE_CALL_GRAPH` | `fp` | On-CPU unwind method. `fp` = frame pointers (soldr is built with `-C force-frame-pointers`; fast fold, but leaves soldr's own Rust frames as `[soldr]`). `dwarf` = `.debug_frame` unwind, which resolves soldr's function symbols — **but the `perf script` fold is very heavy on large captures and can take ~1 h+ on WSL2**, so use it only on a capable runner (and see `SOLDR_PROFILE_FOLD_TIMEOUT`). |
| `SOLDR_PROFILE_FOLD_TIMEOUT` | `600` | Seconds the on-CPU `perf script` fold may run before it is killed and that scenario's chart is skipped (a slow fold degrades gracefully instead of hanging the whole harness). `0` disables the bound. |

The fixture defaults to `perf/fixtures/medium` (the same one the Perf
Matrix uses), so the profile data lines up with the gate-workflow numbers.

## Why the perf-matrix scenarios (vs. the earlier cold/warm/bare loop)

The prior version of this harness ran three ad-hoc scenarios
(`cold` / `warm` / `bare`) that were a straight `soldr cargo build`
against an extracted fixture. Useful for anchoring soldr's dispatch
overhead against a bare-cargo baseline, but not the same measurements
the gate workflow (`perf-matrix.yml`) uses. That mismatch made it hard
to tie profile findings back to regression signal.

The current harness runs the SAME scenarios as `perf-matrix.yml`, so a
flame graph hot spot corresponds directly to a scenario the gate would
flag.

## Comparison to zccache's profile harness

Same base image, same profiler stack (linux-perf + bpftrace + FlameGraph),
same SVG style. Differences:

- **Workload**: perf-matrix scenarios (`bash perf/scenarios/*/run.sh`)
  instead of `cargo test perf_rustc_zccache_vs_sccache`.
- **Top-5 aggregation**: `aggregate_top5.py` post-processes the folded
  stacks and produces `top5.md` — the zccache harness has no equivalent.
- **Same toolchain pin, same samplers** so the two bundles are directly
  comparable side-by-side.
