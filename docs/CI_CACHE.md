# CI Cache Guide For External Repos

This is a usage guide for anyone wiring `zackees/soldr@v1` into their own GitHub Actions CI. It explains what you get automatically, what the minimum config looks like, and how to confirm warm builds on feature branches are actually restoring from `main`.

If you want the background on why this repository wires its own workflows the way it does, skip to [Why This Repo Uses This Model](#why-this-repo-uses-this-model) at the bottom.

## TL;DR

Add `zackees/soldr@v1` to a normal `push`-triggered workflow:

```yaml
- uses: actions/checkout@v4
- uses: zackees/soldr@v1
- run: soldr cargo build --locked
```

You get, for free:

- branch-agnostic cache keys the action produces on its own
- automatic restore on feature branches from the latest `main` cache on a miss
- no separate `actions/cache` step — the action already runs one internally
- a `cache-hit` output you can read to confirm warm vs cold runs

The rest of this document explains how and why that works.

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

The `zackees/soldr@v1` action (see [`action.yml`](../action.yml)) runs an internal `actions/cache` step keyed so that the parent-to-child restore works correctly without you configuring anything:

- **Branch-agnostic cache keys.** The cache key is derived from runner OS, runner architecture, the resolved Rust toolchain channel, and the requested soldr version. No branch name is in the key. Two branches with the same toolchain pin produce the same key, so a cache written by `main` is a valid candidate for a run on any feature branch.
- **Restore-keys prefix for partial-match fallback.** The action registers a restore prefix (`setup-soldr-v1-{os}-{arch}-`) so that even if a future toolchain bump changes the exact key, GitHub can still fall back to the most recent compatible cache for the same OS and architecture.
- **Push-only save semantics come for free.** GitHub's cache scoping already prevents feature-branch runs from overwriting `main`'s cache. You do not need to gate `save-if` yourself the way internal Rust caching wrappers usually make you do.
- **Rehydrated state.** On a cache hit, the action restores the soldr root, `CARGO_HOME`, and `RUSTUP_HOME` under the runner-local cache/state root. The resolved Rust toolchain and the `soldr` binary are then provisioned on top of whatever was restored.

Note that the action does not currently cache the `zccache` compiler artifact directory. Compiler output caching at the `RUSTC_WRAPPER` layer is still managed by zccache's own defaults today.

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

      - uses: zackees/soldr@v1
        with:
          cache: true

      - run: soldr cargo build --locked
      - run: soldr cargo test --locked
```

That is enough. No separate `actions/cache` step, no `Swatinem/rust-cache`, no manual `save-if` gating. The action handles the cache internally with the key shape described above.

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

1. **Check the `cache-hit` output of the setup step.** Reference it from a later step like this:

    ```yaml
    - id: soldr
      uses: zackees/soldr@v1
      with:
        cache: true
    - run: echo "cache-hit=${{ steps.soldr.outputs.cache-hit }}"
    ```

    `true` means the key matched exactly. `false` means either a fresh key (cold) or a restore-keys fallback match (partial). Both `false` cases show the same literal `false` — distinguish them using the raw log.

2. **Open the raw log of the setup step.** Expand the internal `actions/cache` step inside the composite action. You want to see either:
    - `Cache restored from key: setup-soldr-v1-...` — exact match, own-branch or default-branch key hit, or
    - `Cache restored successfully` followed by a key that matches the restore prefix `setup-soldr-v1-{os}-{arch}-` — partial match via restore-keys.
   A line that says no cache was found at all, with no restore match, indicates a cold miss.

3. **Compare wall-clock.** A warm feature-branch run should not rebuild the toolchain or re-download soldr. If you see `rustup` installing or soldr downloading from GitHub Releases on every run, the restore is not happening and something below is wrong.

## Debugging Cold Misses

If feature branches keep rebuilding from scratch, check these in order:

- **Has `main` run successfully recently?** The restore fallback only works if the default branch has written a cache. If the main-branch pipeline is red or was never run on this workflow file, there is no parent to restore from. Fix `main` first.
- **Is `Cargo.lock` churning on every push?** Lockfile changes do not change the setup-soldr cache key, but they do invalidate downstream compilation caches managed by zccache. Check whether your workflow keeps regenerating `Cargo.lock` (for example, because `Cargo.lock` is gitignored in an application repo where it should be committed).
- **Did `rust-toolchain.toml` change?** The resolved toolchain channel is part of the cache key hash. Bumping the toolchain channel or the components/targets list invalidates every existing entry. That is expected behavior; the next push to `main` will write a fresh canonical entry.
- **Did you pass a `cache-key-suffix` input?** That value is appended to the cache key (see `action.yml`). A different suffix on a feature branch produces a different key than `main` writes, and the restore will only succeed through the prefix fallback. Make sure the same suffix is used (or omitted) on every branch you want to share a lineage.
- **Mixed runner OS/arch.** Cache keys are scoped by runner OS and architecture. A cache written on `ubuntu-24.04` will not restore on `macos-15` and vice versa. Each combination needs its own warm lineage from `main`.

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

- `push` runs on `main` and all feature branches.
- The heavy cache-producing CI workflow does not run on `pull_request`.
- `Swatinem/rust-cache` uses a stable `shared-key: workspace`.
- `save-if` is enabled only for `push` events.

In [`.github/workflows/_bootstrap-e2e.yml`](../.github/workflows/_bootstrap-e2e.yml):

- The repo-local `cache-benchmark-zccache` action uses stable target-based keys.
- `save_cache` is passed through from the caller and is `${{ github.event_name == 'push' }}` in `ci.yml`.
- There is no duplicate `pull_request` cache-writing path.

That produces the intended behavior: a push to `main` refreshes the canonical dependency cache, a push to a feature branch saves a branch-local cache in that branch scope, and any PR from that branch surfaces the latest push-run checks instead of a duplicated merge-ref cache lineage.

This repository itself is the reference implementation of that pattern.
