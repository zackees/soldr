# soldr performance cluster

`workflow_dispatch`-triggered GitHub Actions workflow that measures
soldr's cache hit rate, daemon memory, and on-disk footprint across a
matrix of *scenarios* (how the cache flows between invocations) and
*fixtures* (what is being cached).

## Why this exists

A "single speed demo" tells you that soldr is slow, never *why*.  Each
matrix cell in this cluster pins a single failure mode:

| Scenario              | What breaks when this cell turns red                |
| --------------------- | --------------------------------------------------- |
| `cold-tar-untar-warm` | cache archive fidelity (tar/untar round-trip)       |
| `worktree-share`      | `ZCCACHE_PATH_REMAP=auto` injection (issue #352)    |
| `touch-no-change`     | mtime/content-hash robustness (soldr save/load #377) |

## How a worker measures

Each worker (one matrix cell) does the same four things and emits a
single JSON line plus a markdown row to `$GITHUB_STEP_SUMMARY`:

1. **Hit rate** comes from `soldr cache report --json` /
   `soldr session-end --json` — soldr already exposes per-session
   `hits`, `misses`, `compilations`, `hit_rate`, plus per-extension
   rollups when `zccache analyze` is available.
2. **Memory** comes from a bash sidecar that polls
   `ps -o pid,rss,vsz,comm` once per second into a CSV filtered to
   `zccache-daemon|rustc|cargo`. Peak and p95 RSS are computed
   post-hoc from the CSV.
3. **Disk footprint** is `du -sb $SOLDR_CACHE_DIR/cache/zccache` plus
   the size of any intermediate tarball.
4. **Wall time** is wrapped around each build step.

The raw CSV and JSON payloads are uploaded as
`perf-results-<scenario>-<os>` artifacts so you can re-analyse a run
without re-firing the workflow.

## Layout

```
perf/
├── fixtures/
│   ├── medium/           # source-of-truth Rust project (Cargo.toml + src/)
│   ├── medium.tar.gz     # byte-deterministic archive, checked in
│   └── regen.sh          # rebuilds the tarball from the source dir
├── lib/
│   ├── common.sh         # measure::* helpers (rss poller, du, summary)
│   └── extract.sh        # untar a fixture into $WORKDIR
├── scenarios/
│   ├── cold-tar-untar-warm/run.sh
│   ├── worktree-share/run.sh
│   └── touch-no-change/run.sh
└── README.md             # this file
```

## Adding a new fixture

1. `mkdir perf/fixtures/<name>` with a self-contained Rust project.
   The Cargo.toml MUST declare `[workspace]` so it does not get
   folded into the parent soldr workspace.
2. `(cd perf/fixtures/<name> && soldr cargo generate-lockfile)` to
   pin transitive versions.
3. `bash perf/fixtures/regen.sh <name>` to produce the tarball.
4. Commit both the source tree and the new tarball.

## Adding a new scenario

1. `mkdir perf/scenarios/<name>` with a `run.sh` that takes the
   fixture's working directory as its first positional argument and
   writes a single JSON line to stdout.
2. Add the scenario name to the matrix in
   `.github/workflows/perf-cluster.yml`.

## Running locally

```bash
# Extract the fixture into a scratch dir and run one scenario:
WORKDIR=$(mktemp -d)
bash perf/lib/extract.sh medium "${WORKDIR}"
bash perf/scenarios/touch-no-change/run.sh "${WORKDIR}/medium"
```

`SOLDR_DEBUG=1` keeps the raw RSS CSV around (otherwise it is deleted
at the end of the scenario).
