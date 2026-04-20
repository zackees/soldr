# CI Cache Strategy

This repository uses GitHub Actions cache scope the way GitHub actually implements it:

- `main` is the canonical warm-cache source.
- Feature branches can restore from their own branch cache first.
- On a miss, feature branches fall back to the cache lineage from `main`.
- Pull request runs are restore-only. They do not write merge-ref caches as the primary strategy.
- Feature branch pushes can save branch-local caches, which later pushes and PRs for that same branch will prefer automatically.

## Why

GitHub Actions caches are not shared across arbitrary sibling branches. A run can restore from:

- its own branch
- the default branch
- for pull requests, the PR base branch

That means the right model is not "share caches between feature branches". The right model is:

1. `main` stays warm and acts as the shared parent lineage.
2. A feature branch restores from its own cache if it already has one.
3. Otherwise the feature branch restores from `main`.
4. A feature branch push may then save a better branch-local cache for later runs of that same branch.

## How This Repo Is Wired

In [.github/workflows/ci.yml](../.github/workflows/ci.yml):

- `push` runs on `main` and feature branches.
- `pull_request` runs stay enabled for review-time verification.
- `Swatinem/rust-cache` uses a stable `shared-key: workspace`.
- `save-if` is enabled only for `push` events.

That produces the intended behavior:

- a push to `main` refreshes the canonical dependency cache
- a push to `feature/x` saves a branch-local cache in the `feature/x` scope
- a PR from `feature/x` restores from the `feature/x` cache if present
- if `feature/x` has no cache yet, the PR falls back to `main`

In [.github/workflows/_bootstrap-e2e.yml](../.github/workflows/_bootstrap-e2e.yml):

- the repo-local `cache-benchmark-zccache` action uses stable target-based keys
- `save_cache` is enabled only for `push`
- PR runs restore only

So the e2e matrix follows the same policy as the main Rust workspace jobs.

## First Test Case

This repository is the first test case for the cache-sharing model.

Recommended validation flow:

1. Push a change to `main` and let CI complete. This warms the canonical cache.
2. Create a feature branch and push a small follow-up change. The first feature-branch push should restore from `main` and then save a branch-local cache.
3. Push a second small commit to the same feature branch. That run should prefer the branch-local cache.
4. Open or update a PR from the same branch. The PR run should restore from the branch cache first, then fall back to `main` if needed.

## Usage Pattern For Other Repos

Use this same shape when applying Soldr CI caching elsewhere:

- keep cache keys branch-agnostic for correctness-relevant inputs only
- let `push` runs save caches in the current branch scope
- let PR runs restore only
- rely on GitHub's built-in restore order so current-branch caches win over `main`
- treat `main` as the canonical shared parent cache, not sibling branches
