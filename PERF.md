# PERF.md — testing soldr performance

soldr has two performance workflows:

1. **`.github/workflows/perf-matrix.yml`** (the "Perf Matrix") — the regression-gate workflow. Push to a branch with a recognized name and the matrix runs the right cells automatically — no manual `workflow_dispatch` needed.
2. **`.github/workflows/benchmark-stats.yml`** (issue #768) — per-commit trend publishing. Runs on every push to `main`, measures 6 canaries against `perf/fixtures/medium`, and force-publishes to the `benchmark-stats` branch + GitHub Pages.

For the perf-matrix per-scenario design rationale (what each cell proves), see [`perf/README.md`](perf/README.md). For the benchmark-stats canary set + discovery URLs, see the **Per-commit benchmark stats** section near the end of this file.

## How it triggers

The workflow fires on:

1. **`workflow_dispatch`** — the "Run workflow" button in the Actions UI. Dispatch inputs are used verbatim and the branch name is ignored.
2. **`push`** to `main`, `perf/**`, or `evaluate/**`. The branch name is parsed into an effective `(platforms, fixtures, scenarios)` scope (see below). The dispatch inputs are not consulted.
3. **Manual `gh workflow run` CLI** — same as the dispatch button.

The full matrix is always loaded; cells that fall outside the resolved scope skip themselves at the gate step. `main` always runs the full sweep.

## Branch-name convention

Branch syntax: **`perf/<plat>-<fix>-<scen>`** with one short token per axis. `all` is the wildcard at any axis.

### Token mapping

| Axis | Branch token | Real value |
|---|---|---|
| Platform | `linux` | `linux` |
| Platform | `win` | `win` |
| Platform | `mac` | `mac-arm` |
| Fixture | `medium` | `medium` |
| Fixture | `sqlite` | `sqlite-link` |
| Scenario | `cold` | `cold-tar-untar-warm` |
| Scenario | `worktree` | `worktree-share` |
| Scenario | `touch` | `touch-no-change` |
| Scenario | `crossverb` | `build-then-check` |
| any axis | `all` | wildcard — run every value on this axis |

Short tokens keep the branch name unambiguous (the real names contain hyphens that would collide with the axis separator).

### Full scope table

Hierarchical patterns plus two full-sweep aliases. The tables below enumerate `cold` / `worktree` / `touch` for compactness; the `crossverb` scenario follows the same shape — substitute it for the trailing scenario token in any pattern (e.g. `perf/linux-medium-crossverb` is the single cell linux × medium × build-then-check). Anything not in this table (e.g., a developer iteration branch like `perf/cluster-hierarchical-skip`) falls back to a full sweep and emits a `::notice::` so the run is still useful.

#### Aliases — full ride

| Branch | Scope |
|---|---|
| `main` | every platform × fixture × scenario |
| `perf/all` | same as `perf/all-all-all` |

#### Platform = `all` (12)

| Branch | Scope |
|---|---|
| `perf/all-all-all` | full sweep |
| `perf/all-all-cold` | every platform, every fixture, **cold** only |
| `perf/all-all-worktree` | every platform, every fixture, **worktree-share** only |
| `perf/all-all-touch` | every platform, every fixture, **touch-no-change** only |
| `perf/all-medium-all` | every platform, **medium** fixture, every scenario |
| `perf/all-medium-cold` | every platform, **medium**, cold only |
| `perf/all-medium-worktree` | every platform, **medium**, worktree only |
| `perf/all-medium-touch` | every platform, **medium**, touch only |
| `perf/all-sqlite-all` | every platform, **sqlite-link**, every scenario |
| `perf/all-sqlite-cold` | every platform, **sqlite-link**, cold only |
| `perf/all-sqlite-worktree` | every platform, **sqlite-link**, worktree only |
| `perf/all-sqlite-touch` | every platform, **sqlite-link**, touch only |

#### Platform = `linux` (12)

| Branch | Scope |
|---|---|
| `perf/linux-all-all` | **linux** only, every fixture × scenario |
| `perf/linux-all-cold` | linux, every fixture, cold only |
| `perf/linux-all-worktree` | linux, every fixture, worktree only |
| `perf/linux-all-touch` | linux, every fixture, touch only |
| `perf/linux-medium-all` | linux + medium, every scenario |
| `perf/linux-medium-cold` | **single cell**: linux × medium × cold |
| `perf/linux-medium-worktree` | **single cell**: linux × medium × worktree |
| `perf/linux-medium-touch` | **single cell**: linux × medium × touch |
| `perf/linux-sqlite-all` | linux + sqlite-link, every scenario |
| `perf/linux-sqlite-cold` | **single cell**: linux × sqlite-link × cold |
| `perf/linux-sqlite-worktree` | **single cell**: linux × sqlite-link × worktree |
| `perf/linux-sqlite-touch` | **single cell**: linux × sqlite-link × touch |

#### Platform = `win` (12)

| Branch | Scope |
|---|---|
| `perf/win-all-all` | **win** only, every fixture × scenario |
| `perf/win-all-cold` | win, every fixture, cold only |
| `perf/win-all-worktree` | win, every fixture, worktree only |
| `perf/win-all-touch` | win, every fixture, touch only |
| `perf/win-medium-all` | win + medium, every scenario |
| `perf/win-medium-cold` | **single cell**: win × medium × cold |
| `perf/win-medium-worktree` | **single cell**: win × medium × worktree |
| `perf/win-medium-touch` | **single cell**: win × medium × touch |
| `perf/win-sqlite-all` | win + sqlite-link, every scenario |
| `perf/win-sqlite-cold` | **single cell**: win × sqlite-link × cold |
| `perf/win-sqlite-worktree` | **single cell**: win × sqlite-link × worktree |
| `perf/win-sqlite-touch` | **single cell**: win × sqlite-link × touch |

#### Platform = `mac` (mac-arm) (12)

| Branch | Scope |
|---|---|
| `perf/mac-all-all` | **mac-arm** only, every fixture × scenario |
| `perf/mac-all-cold` | mac, every fixture, cold only |
| `perf/mac-all-worktree` | mac, every fixture, worktree only |
| `perf/mac-all-touch` | mac, every fixture, touch only |
| `perf/mac-medium-all` | mac + medium, every scenario |
| `perf/mac-medium-cold` | **single cell**: mac × medium × cold |
| `perf/mac-medium-worktree` | **single cell**: mac × medium × worktree |
| `perf/mac-medium-touch` | **single cell**: mac × medium × touch |
| `perf/mac-sqlite-all` | mac + sqlite-link, every scenario |
| `perf/mac-sqlite-cold` | **single cell**: mac × sqlite-link × cold |
| `perf/mac-sqlite-worktree` | **single cell**: mac × sqlite-link × worktree |
| `perf/mac-sqlite-touch` | **single cell**: mac × sqlite-link × touch |

> Today, only `linux` has a matrix row in `build-soldr` / `bench`. `win` and `mac` branches resolve correctly via setup but their cells gate out until cross-platform runner rows land. The branch names stay stable.

## Picking a branch for the work you're doing

- **Iterating on cache hit-rate fixes that only affect sqlite builds** → `perf/linux-sqlite-cold` (fastest signal: one cell, the hard gate scenario).
- **Tuning archive fidelity** → `perf/all-all-cold` (sweep cold-tar-untar-warm across everything; fixture variation matters).
- **Worktree path-remap change** → `perf/linux-all-worktree` (every fixture on linux, worktree scenario only).
- **Cross-verb cache canonicalization (e.g. `--emit=metadata` ↔ `--emit=metadata,link`)** → `perf/linux-medium-crossverb` (single cell that pins the build→check asymmetry from #758).
- **Just experimenting / unsure** → `perf/all` or `main` — full sweep; the workflow handles the volume.
- **Personal feature branch like `perf/wip/foo`** → falls through to full sweep with an `::notice::`. Fine for one-off runs; rename to a canonical pattern when you know what axis you're working on.

## Gate semantics

- **`cold-tar-untar-warm` < 3x** (cold/warm ratio in the Evaluate step) → **fails the workflow**. Hard gate.
- **`worktree-share` < 3x** → emits `::warning::`, doesn't fail. Soft gate today; promotes to hard once the baseline stabilizes.
- **`touch-no-change` < 3x** → same as worktree-share, soft today.
- **`build-then-check` < 3x** → soft warning. The "warm" step is `cargo check` after a warm `cargo build` (same source, same profile); today it returns 0% hits because zccache's cache key splits on the rustc `--emit` flag, so the ratio is close to the threshold. Promotes to hard once zccache canonicalizes `--emit=metadata` against `--emit=metadata,link` keys. Tracks #758.

Threshold lives on the `evaluate` matrix row (`min_speedup: "3.0"`).

## Reading the run

Every cell appends to `$GITHUB_STEP_SUMMARY`. From the run page:

1. **Scope** table at the top (`setup` job) — confirms the resolved `(platforms, fixtures, scenarios)` and the source (`branch:<ref>`, `alias:main`, `dispatch`, `unknown-perf:<ref>`, etc.).
2. **bench** cells emit a per-fixture table with `cold/A ms | warm/B ms | speedup | hits/misses | hit rate | peak daemon RSS`.
3. **Evaluate** cell emits a single per-platform table covering every (fixture, scenario) it could find, with `cold | warm | speedup | threshold | mode | result`.
4. Failed runs annotate the failing rows with `::error::` lines (visible in the "Annotations" sidebar).

Raw `result.json`, `*-shutdown.json`, and `rss-*.csv` are uploaded as `perf-results-<platform>-<fixture>` artifacts (14-day retention).

## Local dry-runs

You can run any single scenario locally without GHA:

```bash
# Set up the fixture
bash perf/lib/extract.sh medium /tmp/perf-medium

# Run one scenario (writes result.json to stdout)
bash perf/scenarios/cold-tar-untar-warm/run.sh /tmp/perf-medium/medium
```

The scripts are POSIX bash and do not require any GHA-only env vars; `measure::append_summary_md` is a no-op when `$GITHUB_STEP_SUMMARY` is unset.

## Per-commit benchmark stats (issue #768)

The Perf Matrix above is the **regression gate** — it runs on perf-iteration branches and hard-fails if `cold-tar-untar-warm` drops below 3× speedup. It does NOT publish a permanent record.

A complementary workflow, `benchmark-stats.yml`, publishes a permanent **per-main-commit canary record** for trend tracking. It runs on every push to `main`, measures 6 canaries against `perf/fixtures/medium`, and force-publishes the results to the `benchmark-stats` branch + GitHub Pages.

### Discovery URLs

Every artifact's URL is listed inside `manifest.json`. Start there:

```
https://raw.githubusercontent.com/zackees/soldr/benchmark-stats/manifest.json
```

The manifest points at:

| Artifact | URL pattern |
|---|---|
| Rich snapshot of latest run | `https://raw.githubusercontent.com/zackees/soldr/benchmark-stats/latest.json` |
| Slim rolling history (1000 lines) | `https://raw.githubusercontent.com/zackees/soldr/benchmark-stats/history.jsonl` |
| Human-facing Chart.js page | `https://zackees.github.io/soldr/` |

### Canary set

| Canary | What it measures | Theoretical (ms) |
|---|---|---|
| `cargo-build-medium-cold` | full cold compile of `perf/fixtures/medium` | 60000 |
| `cargo-build-medium-warm` | immediate repeat (cargo freshness fast-path) | 500 |
| `cargo-build-medium-from-warm-zccache` | `cargo clean` + rebuild from warm zccache | 10000 |
| `cargo-check-medium-cross-verb` | build → check; pins #758 / [zccache#776](https://github.com/zackees/zccache/issues/776) | 1500 |
| `touch-no-change-medium-warm` | source mtimes bumped, content unchanged; expect 100% hits | 1500 |
| `worktree-share-medium-warm` | second fixture extraction, same `SOLDR_CACHE_DIR` | 1500 |

### Branch shape

The `benchmark-stats` branch is **not orphan** — it carries a rolling 50 commits of git history (shallow-clone-then-force-push pattern in `bench/publish_benchmark_stats.sh`). Older commits become unreferenced and GitHub garbage-collects them.

The slim `history.jsonl` file (one line per commit) accumulates to **1000 lines** total before the oldest entry is dropped. So the data carries far longer-running trends than git's 50-commit window.

### Workflow files

- `.github/workflows/benchmark-stats.yml` — the workflow
- `bench/run_canaries.sh` — runs the 6 canaries
- `bench/assemble_benchmark_stats.sh` — fetches prior history, writes `manifest.json` + `latest.json` + `history.jsonl` + `index.html`
- `bench/publish_benchmark_stats.sh` — shallow-clone + force-push to `benchmark-stats`
- `bench/index.html` — Chart.js page template (copied verbatim into the publish dir)
