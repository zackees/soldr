# CI Cache Guide For External Repos

This is a usage guide for anyone wiring `zackees/setup-soldr@v0` into their own GitHub Actions CI. It explains what you get automatically, what the minimum config looks like, and how to confirm warm builds on feature branches are actually restoring from `main`.

If you want the background on why this repository wires its own workflows the way it does, skip to [Why This Repo Uses This Model](#why-this-repo-uses-this-model) at the bottom.

## TL;DR

Add `zackees/setup-soldr@v0` to a normal `push`-triggered workflow:

```yaml
- uses: actions/checkout@v4
- uses: zackees/setup-soldr@v0
- run: soldr cargo build
```

You get, for free:

- branch-agnostic cache keys the action produces on its own
- automatic restore on feature branches from the latest `main` cache on a miss
- no separate `actions/cache` step; the action already runs the setup-state cache internally and also restores and saves the Soldr-owned zccache cache root and the zccache-owned Rust artifact plan cache by default
- `cache-hit`, `build-cache-hit`, and `target-cache-hit` outputs you can read to confirm warm vs cold runs

The rest of this document explains how and why that works.

> **Deprecated (soldr#2996).** soldr no longer implements a target cache. The
> `target-cache*` inputs and outputs described below are inert on the soldr
> side; `soldr cook` is the only durable compiler cache. Retiring the inputs
> themselves is an upstream change to the action.

## Cache Ownership And Priority

Cache admission is decided by **the stability of the artifact's identity key
relative to its size**. Cache what invalidates rarely; never persist what
invalidates on every edit. The gradient, from most to least cacheable:

| Artifact class | Invalidates when | Owner |
|---|---|---|
| Toolchain / SDK / catalogue downloads | pinned version bumps (rare, explicit) | setup-state cache (content-pinned + sha256) |
| External dependency compilation | `Cargo.lock` / recipe changes | **soldr cook** (Tier 1) |
| Per-compilation-unit compiler outputs | that unit's input hash changes | **zccache store** (Tier 2, content-addressed) |
| Workspace linked products — final binaries and especially **test executables** | **any** workspace source edit | **nothing** (Tier 3 — never cached) |

**Linked test products are never cacheable** — test binaries, benches,
examples built for tests, doctest products, test debug sidecars, and
test-specific incremental state. They are the most volatile artifacts a
workspace produces (every source edit re-links them) and the largest (each
integration-test file is its own executable statically linking the full
dependency graph). Classification is by contents, not by whether GitHub calls
the store a "cache" or an "artifact". A same-run transport bundle used to
execute cross-built tests on their target is not a cache, but it must stay
compact, single-extraction, and budgeted.

Lesson learned (soldr#2931): this repo's suite reached ~110 linked test
binaries and a 3.3 GB compressed nextest archive that exhausted a hosted
runner's disk, while a guard *required* that archive to be warm-cacheable at
zero misses. Every individual decision on the way there was locally
reasonable; the missing piece was this ownership table to check against. When
adding a cache layer, name its tier first — if the payload contains linked
test products, the answer is no.

## How GitHub Actions Cache Scoping Actually Works

A workflow run in GitHub Actions can restore caches from a limited set of scopes, and **not from arbitrary sibling branches**. For any given run, GitHub will consider caches in this order:

1. The run's own branch
2. For `pull_request` events, the PR base branch
3. The repository's default branch (usually `main`)

That means two feature branches cannot share a cache entry directly. The only way to get a shared lineage is to treat the default branch as a shared parent: `main` writes caches, feature branches read them on miss.

Authoritative reference: [Caching dependencies to speed up workflows](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/cache-dependencies).

Two consequences of that scoping rule matter for soldr:

- **`main` is the canonical warm source.** Keep `main` passing so it refreshes its cache entries on every push. A broken `main` pipeline means cold feature-branch builds.
- **Saves are own-branch only.** A run on `feature/x` cannot write into `main`'s cache scope, and it cannot write into `feature/y`'s cache scope. It saves into its own branch scope, and later runs on that same branch restore it first.

## What setup-soldr Does For You Automatically

The `zackees/setup-soldr@v0` action (generated from [`action.yml`](../action.yml)) runs internal cache steps keyed so that the parent-to-child restore works correctly without you configuring anything:

- **Branch-agnostic state-cache keys.** The setup-state cache key is derived from runner OS, runner architecture, the resolved Rust toolchain channel, and the requested soldr version. No branch name is in the key. Two branches with the same toolchain pin produce the same key, so a cache written by `main` is a valid candidate for a run on any feature branch.
- **Restore-keys prefix for partial-match fallback.** The action registers a restore prefix (`setup-soldr-v0-{os}-{arch}-`) so that even if a future toolchain bump changes the exact key, GitHub can still fall back to the most recent compatible cache for the same OS and architecture.
- **Push-only save semantics come for free.** GitHub's cache scoping already prevents feature-branch runs from overwriting `main`'s cache. You do not need to gate `save-if` yourself the way internal Rust caching wrappers usually make you do.
- **Rehydrated state.** On a cache hit, the action restores the soldr root, `CARGO_HOME`, and `RUSTUP_HOME` under the runner-local cache/state root. The resolved Rust toolchain and the `soldr` binary are then provisioned on top of whatever was restored.
- **Build-artifact cache enabled by default.** The action also restores the Soldr-owned zccache cache root with a toolchain-scoped key and saves it at end-of-job, so zccache compilation artifacts survive across runs unless you opt out with `build-cache: false`.
- **Thin Rust artifact cache enabled by default.** The action restores a zccache-owned Rust artifact plan cache when a `Cargo.lock` is present. `soldr cargo ...` generates a `thin` plan by default and asks zccache to restore/save bounded dependency artifacts. It does not use an action-owned full `target/` snapshot unless the workflow explicitly sets `target-cache-mode: full`, which is still executed by zccache from the soldr-generated plan.

Release/LTO musl validation and daemon-failure diagnostics live in
[`docs/DATALAKE_RELEASE_MUSL.md`](DATALAKE_RELEASE_MUSL.md).

## Minimum Config For An External Repo

This is the complete workflow. Copy-paste into `.github/workflows/ci.yml` and adjust the job matrix if you need more than Linux:

```yaml
name: CI

on:
  push:
    branches: ['**']

permissions:
  contents: read

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: zackees/setup-soldr@v0
        with:
          cache: true

      - run: soldr cargo build
      - run: soldr cargo test
```

That is enough. No separate `actions/cache` step, no `Swatinem/rust-cache`, no manual `save-if` gating. The action handles the cache internally with the key shapes described above.

A slightly fuller example that also demonstrates reading the action's outputs lives in [`examples/ci-minimal.yml`](../examples/ci-minimal.yml).

## Triggering On `push` Vs `pull_request`

Prefer `push` on all branches as your default trigger:

```yaml
on:
  push:
    branches: ['**']
```

Why:

- A `push` run on `feature/x` writes its cache into the `feature/x` scope. The next push on that branch restores it first. PRs from `feature/x` implicitly see those checks because they are tied to the branch head.
- A `pull_request` run builds the synthetic merge commit, writes into a PR-specific cache lineage, and duplicates the work that the branch push already did. You end up with two lineages competing for the same build time.
- Adding `pull_request` on top of `push` roughly doubles CI minutes and creates a second cache entry that does not help feature-branch restore from `main`.

Add `pull_request` only if you explicitly need CI on the PR merge commit (for example, a required merge-ref signal that a forked-branch push cannot produce). Most repos do not need this.

## Verifying The Cache Is Working

After two pushes to the same branch, you should be able to confirm the cache lineage is healthy.

1. **Check the `cache-hit`, `build-cache-hit`, and `target-cache-hit` outputs of the setup step.** Reference them from a later step like this:

   ```yaml
   - id: soldr
     uses: zackees/setup-soldr@v0
     with:
       cache: true
   - run: echo "cache-hit=${{ steps.soldr.outputs.cache-hit }}"
   - run: echo "build-cache-hit=${{ steps.soldr.outputs.build-cache-hit }}"
   - run: echo "target-cache-hit=${{ steps.soldr.outputs.target-cache-hit }}"
   ```

   `true` means the key matched exactly. `false` means either a fresh key (cold) or a restore-keys fallback match (partial). Both `false` cases show the same literal `false`; distinguish them using the raw log.

2. **Open the raw log of the setup step.** Expand the internal cache steps inside the composite action. You want to see either:
   - `Cache restored from key: setup-soldr-v0-...` for an exact setup-state cache hit, or
   - `Cache restored successfully` followed by a key that matches the restore prefix `setup-soldr-v0-{os}-{arch}-` for a partial setup-state restore.

   A line that says no cache was found at all, with no restore match, indicates a cold miss.

   For the build-artifact layer, inspect the `build-cache-restore` step. Its exact keys are `setup-soldr-buildcache-v1-{os}-{arch}-{toolchain-digest}-{github.sha}` and its restore-keys fall back first to the same toolchain lineage, then to any cache for the same OS and architecture.

   For the Rust artifact plan layer, inspect the `target-cache` step. Its default thin-cache keys are `setup-soldr-targetcache-thin-v2-{os}-{arch}-{target-inputs-hash}` and use exact restore only. The target-inputs hash includes the toolchain digest, `Cargo.lock`, workspace manifest hashes, Cargo config, target-dir shape, and relevant Rust flags.

3. **Compare wall-clock.** A warm feature-branch run should not rebuild the toolchain or re-download soldr. A warm build-artifact restore should also reduce downstream compile time once zccache has artifacts to reuse. If you see `rustup` installing, soldr downloading from GitHub Releases, or full recompiles on every run, one of the restore layers is not hitting and something below is wrong.

4. **Inspect zccache stats after the build.** Add a post-build status step when validating a new cache lineage:

   ```yaml
   - run: soldr cache
   ```

   The output includes the embedded zccache cache root and daemon-reported
   counters. For a healthy warm build, look for non-zero cached compilations
   or hit counts, plus the `zccache rust-plan restore/save` JSON summaries
   emitted by `soldr cargo ...`. If `build-cache-hit=true` but zccache still
   reports zero cached compilations, the build-artifact cache restored but did
   not produce compiler-cache reuse; check whether the target-cache layer also
   restored and whether Cargo invalidated fingerprints before zccache could
   hit.

## Debugging Cold Misses

If feature branches keep rebuilding from scratch, check these in order:

- **Has `main` run successfully recently?** The restore fallback only works if the default branch has written a cache. If the main-branch pipeline is red or was never run on this workflow file, there is no parent to restore from. Fix `main` first.
- **Is `Cargo.lock` churning on every push?** Lockfile changes do not change the setup-soldr state-cache key, but they do invalidate the Rust artifact plan cache and can reduce downstream zccache reuse. Check whether your workflow keeps regenerating `Cargo.lock` (for example, because `Cargo.lock` is gitignored in an application repo where it should be committed).
- **Did `rust-toolchain.toml` change?** The resolved toolchain channel is part of both cache key families. Bumping the toolchain channel or the components/targets list invalidates every existing entry. That is expected behavior; the next push to `main` will write a fresh canonical entry.
- **Did you pass a `cache-key-suffix` input?** That value is appended to both cache key families (see `action.yml`). A different suffix on a feature branch produces a different key than `main` writes, and the restore will only succeed through the prefix fallback. Make sure the same suffix is used (or omitted) on every branch you want to share a lineage.
- **Mixed runner OS/arch.** Cache keys are scoped by runner OS and architecture. A cache written on `ubuntu-24.04` will not restore on `macos-15` and vice versa. Each combination needs its own warm lineage from `main`.
- **Did someone opt out of build caching?** If `build-cache: false` is set in the workflow, `build-cache-hit` will be empty and the Soldr-owned zccache cache root will not be restored or saved.
- **Did someone opt out of target caching?** If `target-cache: false` is set in the workflow, `target-cache-hit` will be empty and soldr will not ask zccache to restore or save Rust target artifacts.

---

## Why This Repo Uses This Model

The rest of this document is ancillary context about how this repository's own CI is wired. External consumers do not need any of this.

GitHub Actions caches are not shared across arbitrary sibling branches. A run can restore from:

- its own branch
- the default branch
- for pull requests, the PR base branch

So the right model is not "share caches between feature branches". The right model is:

1. `main` stays warm and acts as the shared parent lineage.
2. A feature branch restores from its own cache if it already has one.
3. Otherwise the feature branch restores from `main`.
4. A feature-branch push may then save a better branch-local cache for later runs of that same branch.

## How This Repo Is Wired

In [`.github/workflows/ci.yml`](../.github/workflows/ci.yml):

- `push` runs on `main` only. Feature branches are covered by the
  `pull_request` trigger instead — soldr#1985 narrowed the push trigger
  because `branches-ignore` matched every PR branch, so each push to an open
  PR ran the whole sweep twice.
- The heavy cache-producing CI workflow therefore *does* run on
  `pull_request`; that is the only way a feature branch gets coverage.
- `Swatinem/rust-cache` survives in exactly three ordinary CI lanes
  (soldr#3047 removed it from every other one): `_bootstrap-e2e.yml`, `ci.yml`'s
  bootstrap driver build, and `ci.yml`'s wheel-cross build. The
  cache-experiment workflows (`baseline-zero-deps.yml`,
  `parent-cache-bench.yml`, `perf-matrix.yml`) keep their own rust-cache steps
  because the cache is the subject under test there; they are budgeted under
  `experiment-lanes` rather than gated. Each of the three ordinary lanes sets
  `save-if: ${{ github.ref == 'refs/heads/main' }}`, so a PR run restores
  whatever `main` last wrote and never saves its own branch-scoped copy — the
  opposite of the `shared-key:`-only, no-`save-if:` posture this repo used to
  have, and deliberate: every open PR used to carry its own copy of the
  entry, which is what pushed the repository over its cache budget (see
  [Repository cache budget](#repository-cache-budget-soldr3047) below).
  `actions/cache` itself has no `save-if` input, so the two main-gated
  `actions/cache` stores — the Dylint foundation trees and the dogfood
  zccache store — get the same effect by splitting into an unconditional
  `actions/cache/restore` followed by an `actions/cache/save` step gated on
  `github.ref == 'refs/heads/main'`.

### The Tier-2 object store is persisted, not just isolated (soldr#3041)

In [`.github/workflows/_build-and-test.yml`](../.github/workflows/_build-and-test.yml)
the host validation lane points `SOLDR_CACHE_DIR` at a per-target directory
under `runner.temp` so every step in the lane shares one wrapper and one
compiler cache. That choice is about *isolation*; `runner.temp` also threw the
store away at the end of every run, so the lane's only per-unit cache was cold
every time. An `actions/cache` step now persists it:

- **Path is selected positively**: `…/cache/zccache/daemon-state` — the
  embedded service's object store
  (`daemon-state/embedded-v1/v<VERSION>/`). The compile journals
  (`cache/zccache/history/`) and session logs (`cache/zccache/logs/`) are
  siblings *outside* that directory and stay run-scoped, shipped by the
  build-logs artifact. Do not switch this to `cache/zccache` plus `!…/history`
  negations: `actions/cache` globs `path` with `implicitDescendants: false`
  and hands the matched directory to `tar`, which recurses, so the negation
  removes nothing. What the positive selection does *not* exclude is the
  service's own versioned log directory
  (`daemon-state/embedded-v1/v<VERSION>/logs/`), which is inside the store and
  rides along in the entry; zccache owns its rotation, and a `path` negation
  could not drop it either.
- **The key rolls**: it ends in `${{ github.run_id }}`, because
  `actions/cache` never re-saves on an exact-key hit — a stable key would
  freeze the store after its first save. Two `restore-keys` fall back to the
  same dependency graph first, then to any earlier entry.
- **Runtime coordination state is scrubbed first**: a step after
  `Stop canonical host CI cache` deletes `staging/` plus every
  `.lock` / `.sock` / `.socket` / `.pid` file under `daemon-state`, mirroring
  `archive_always_excludes_cache_path` in
  `crates/soldr-cache/src/cache_lib/save_inventory.rs`. Soldr's own archive
  transport already refuses to collect those files: the daemon deletes its
  `.active.lock` between walk and stat, which killed a real `soldr save`, and
  `tar` shares that race. A failed cache save is only a *warning*, so without
  the scrub the entry could silently never be written while the lane stayed
  green — and a restored stale lock or socket is documented in that module as
  preventing the compile daemon from starting.
- **Tier**: `zccache-unit`, already declared durable in
  `ci/cache-ownership.json`. This is not a new cache family; content-addressed
  per-unit caching was never what soldr#2931 banned.

### Cache key scheme (soldr#1978 item 6)

Where a rust-cache namespace still exists, it is keyed on **(profile,
target)**, not on the job:

```
shared-key: ws-<profile>-<target>
```

The point is that lanes compiling the same dependency graph, at the same
profile, for the same triple should share one namespace instead of each
owning a private one.

The dev-profile instance of that scheme is **gone**, and the reason is worth
keeping: `ws-dev-<target>` promised one `target/` restore for the native
validation lane, but soldr#2996 measured the `Swatinem/rust-cache` step
serving it at a 0% hit rate — the key's environment hash covers every
installed toolchain, so it flipped with the Dylint nightly on every run.
soldr#3047 deleted the step rather than re-key it; the successor is the
Tier-2 per-unit object store (soldr#3041) plus the workflow-level `soldr
cook` step (soldr#3043), not another shared-key namespace, and
`tests/test_ci_cache_key_scheme.py` now pins the namespace's *absence*. The
surviving `ws-release-*` pair lives in `baseline-zero-deps.yml`, where one
job populates and the next restores inside a cache-experiment workflow.

Two constraints make this less mechanical than it looks:

- **A shared key is worthless unless the sharers write the same directory.**
  Cargo puts a bare build in `target/debug` and a `--target`-qualified build
  in `target/<triple>/debug`. `lint` had no `--target`, so it shared nothing
  with `build-linux-x64` no matter what the key said. Both now pass
  `--target`, and `tests/test_ci_cache_key_scheme.py` pins the flag and the
  key together.
- **Performance and cache-strategy lanes stay off the scheme.**
  `perf-matrix.yml`, `perf-cold-warm.yml`, `parent-cache-bench.yml`, and
  `cache-delta-experiment.yml` keep private, workflow-scoped namespaces. They
  either measure timings that an unrelated lane's writes would contaminate,
  or compare cache strategies. `perf-cold-warm.yml` additionally purges every
  repo cache whose key *contains* `perf-cold-warm`, so its key must keep that
  substring and no shared key may contain it — both pinned by the same test.

Note that `rust-cache`'s key does not include Cargo profile *definitions*, so
a change to a `[profile.*]` table does not invalidate these caches on its own;
that is why the profile is spelled out in the key text.

This repository itself is the reference implementation of that pattern.

## Repository cache budget (soldr#3047)

GitHub's Actions cache has a hard 10 GB ceiling per repository. Nothing asks
permission before that ceiling is reached — GitHub silently evicts the
least-recently-used entries to make room, so a producer that writes a lot but
is rarely read can quietly starve a producer that is small but read on every
run. That makes the ceiling a shared-resource problem, not a per-workflow one:
a persisted object store (soldr#3041) and cook archives (soldr#3042/#3043) are
worthless if an unrelated lane's growth evicts them before they are ever
restored.

Measured on 2026-09-01, before this PR: 47,489,917,871 bytes (44.23 GiB)
across 143 entries — more than four times the ceiling. 23.79 GiB of that was
`Swatinem/rust-cache` alone, and the reason was structural rather than a size
problem with any one entry: nothing restricted saves to `main`, so every
open PR wrote its own branch-scoped copy of every rust-cache producer, and
those copies never got evicted fast enough to matter.

The fix has two parts. First, stop writing what does not need to exist:
`cross-build-rust-cache`, `build-and-test-rust-cache`, `ci-pep517-rust-cache`,
and `target-run-pep517-rust-cache` are retired outright (their `setup-soldr
cook` or plain "run uncached" replacements are cheaper than a rust-cache
entry every PR pays for and few ever hit). What's left is one surviving
`cook-unreachable-lane` exception — `ci.yml`'s bootstrap driver build, which
runs bare `cargo build` with `RUSTC_WRAPPER` emptied and has no setup-soldr
step, because it is the job that *builds* soldr and handing it a soldr to
cook with would reintroduce the prebuilt-binary coupling soldr#2451 forbids.
Second, gate what remains to `main`-only saves (see [How This Repo Is
Wired](#how-this-repo-is-wired) above) so a PR restores but never writes.

What survives that sweep is budgeted, not merely trimmed, because "smaller"
is not the same guarantee as "under the ceiling forever." `ci/cache-ownership.json`
carries a top-level `budget` map: one entry per producer family, each with a
`key_prefixes` list, a `max_bytes` allocation, and the family's measured
live-on-`main` size. Every allocation is `>=` that measured size, and the
family allocations sum to exactly 9 GiB (`total_max_bytes`). The gate's hard
ceiling, `fail_total_bytes`, is GitHub's documented 10 GB read as decimal
bytes (10,000,000,000) — GitHub does not publish which byte-multiple it
means, and the decimal reading is at or under the real limit either way, so
the gate can never pass a store GitHub is already evicting:

| Family | Allocation | Covers |
|---|---|---|
| `pinned-immutable-download` | 1.2 GiB | rustup, xwin SDK, Apple SDK, solo-toolchain, soldr-mini, setup-uv |
| `bootstrap-driver-binary` | 0.15 GiB | the per-commit-SHA bootstrap driver |
| `cook-layer` | 2.0 GiB | `setup-soldr/cook` archives for the cross lanes |
| `dylint-foundation` | 0.8 GiB | the ci-test Dylint foundation + analysis trees |
| `rust-cache-residual` | 1.4 GiB | the three `Swatinem/rust-cache` producers left after this PR |
| `setup-soldr-action-stores` | 1.0 GiB | per-unit zccache stores + the action's own registry slice |
| `experiment-lanes` | 0.45 GiB | workflows where the cache is the subject under test |
| `zccache-unit` | 2.0 GiB | reserved for the Tier-2 object store soldr#3041 persists |

`ci/cache-ownership.json`'s `budget` map is the single source of truth for
this accounting: `.github/scripts/check_cache_budget.py` and
`.github/scripts/check_cache_ownership.py` both read it rather than each
carrying their own copy of the family list. `check_cache_budget.py` fails the
build if a cache key does not match any registered `key_prefixes` entry — a
new producer cannot appear without being registered in the same PR that adds
it, and cannot be silently folded into an existing family's headroom by
accident. Raising `total_max_bytes` is not one of the available levers; the
only way to make room for a new producer is to retire or shrink an existing
one.

GitHub's own LRU eviction is not under this repo's control and does not
consult the family allocations above, so the budget guard pairs with a
`--prune` sweep that brings the live store back inside those allocations
directly instead of waiting on GitHub's schedule; see the script's own
docstring for the selection policy. See `ci/cache-ownership.json`'s
`budget.comment` field for the exact measurement this table was derived from,
including the one issue-scope adjustment (the pep517 rust-cache) made after
the original soldr#3047 step-1 list was written.
