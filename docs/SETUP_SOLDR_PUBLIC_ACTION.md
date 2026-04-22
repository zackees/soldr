# setup-soldr Public Action Plan

This document is the repo-contained delivery for [issue #137](https://github.com/zackees/soldr/issues/137).

`soldr` cannot publish its current root action to GitHub Marketplace directly. This document freezes the public `setup-soldr` contract, defines the extraction boundary, and documents the release process that turns the current in-repo action into the separate public action product at [`zackees/setup-soldr`](https://github.com/zackees/setup-soldr).

## Why Extraction Is Required

GitHub's current action guidance says public actions are best kept in their own repository, and GitHub Marketplace requires:

- a public repository
- a single root `action.yml` or `action.yaml`
- no workflow files in the published repository

References:

- https://docs.github.com/en/actions/how-tos/sharing-automations/creating-actions/publishing-actions-in-github-marketplace
- https://docs.github.com/actions/how-tos/creating-and-publishing-actions/managing-custom-actions

This repository intentionally contains normal application code and workflow files under `.github/workflows/`, so it is the source repository for the action, not the Marketplace repository.

## Current Source Of Truth

The current setup action that should be extracted is:

- [action.yml](../action.yml)
- `.github/actions/setup-soldr/resolve_setup.py`
- `.github/actions/setup-soldr/ensure_rust_toolchain.py`
- `.github/actions/setup-soldr/ensure_soldr.py`
- `.github/actions/setup-soldr/verify_soldr.py`

The current smoke validation stays in this repository and is not copied into the public action repository:

- `.github/workflows/setup-soldr-action.yml`

That split is intentional. The Marketplace repository stays minimal, while this repository keeps the broader test and release context.

## Planned Public Repository Shape

Create a separate public repository, expected name:

- `zackees/setup-soldr`

Initial extraction should keep the helper-script layout unchanged so the copy is mechanical:

```text
setup-soldr/
|-- action.yml
|-- README.md
`-- .github/
    `-- actions/
        `-- setup-soldr/
            |-- resolve_setup.py
            |-- ensure_rust_toolchain.py
            |-- ensure_soldr.py
            `-- verify_soldr.py
```

Do not copy any file from `.github/workflows/`.

## Beta Public `v0` Contract

The intended public beta UX is:

```yaml
steps:
  - uses: actions/checkout@v4

  - uses: zackees/setup-soldr@v0
    with:
      cache: true

  - run: soldr cargo build --locked --release
  - run: soldr cargo test --locked
```

### Supported Inputs

`v0` should treat these inputs as the beta public contract:

| Input | Meaning |
|---|---|
| `version` | Soldr release tag or version to install. Empty means latest release. |
| `cache` | Restore and save the action-managed cache/state root. |
| `cache-dir` | Override the runner-local cache/state root. |
| `cache-key-suffix` | Optional escape hatch appended to the cache key. |
| `toolchain` | Explicit Rust toolchain channel override. |
| `toolchain-file` | Alternate toolchain file path when `toolchain` is empty. |
| `trust-mode` | Optional `SOLDR_TRUST_MODE` value. |
| `build-cache` | Restore and save the Soldr-owned zccache compilation artifact cache across runs. Default `"true"`; set to `"false"` to opt out. |
| `target-cache` | Restore and save the Cargo target directory for no-op CI fast paths. Default `"true"`; set to `"false"` to cache only zccache compilation artifacts. |
| `target-dir` | Cargo target directory restored by `target-cache`. Default `"target"`. |

The current in-repo action also exposes `repo` as an implementation/testing override. That input is not part of the intended public `v0` beta contract and should not be documented in the extracted public action README.

### Supported Outputs

`v0` should treat these outputs as the beta public contract:

| Output | Meaning |
|---|---|
| `soldr-path` | Installed Soldr binary path added to `PATH`. |
| `soldr-version` | Installed Soldr version reported by `soldr version --json`. |
| `cache-dir` | Action-managed runner-local cache/state root. |
| `cache-hit` | Whether the action restored an exact cache hit. |
| `build-cache-hit` | Whether the Soldr-owned zccache compilation cache was restored. Empty only when `build-cache` is explicitly disabled. |
| `target-cache-hit` | Whether the Cargo target directory cache was restored. Empty only when `build-cache` or `target-cache` is explicitly disabled. |
| `toolchain` | Exact Rust toolchain channel configured for the action. |

### Required Behavior

`v0` should preserve these behaviors:

- install exactly one released Soldr binary for the active runner OS and architecture
- provision the normal-path Rust toolchain itself, bootstrapping `rustup` via `rustup-init` when it is not already on the runner; users should not need to preinstall `rustup` or run a separate toolchain-setup action for the common path
- create and export `SOLDR_CACHE_DIR`, `CARGO_HOME`, and `RUSTUP_HOME`
- put the installed `soldr` binary on `PATH`
- restore and save the action-managed cache/state root when `cache: true`
- export `RUSTUP_TOOLCHAIN` after toolchain installation so later `cargo`, `rustc`, and `soldr cargo ...` steps stay on the same resolved toolchain
- when `build-cache: true` (the default), restore the Soldr-owned zccache cache root at setup time and save it at end-of-job (`if: always()`) so subsequent runs rehydrate zccache compilation artifacts. Keys are `setup-soldr-buildcache-v1-{os}-{arch}-{toolchain-digest}-{github.sha}` with restore-keys that first fall back to the same `{toolchain-digest}` lineage, then any cache for the same `{os}-{arch}`. GitHub's own-branch -> PR base -> default-branch restore order seeds feature-branch runs from the latest main-branch save without user configuration. Consumers that explicitly do not want cross-run cache reuse can set `build-cache: false`.
- when `build-cache: true` and `target-cache: true` (the defaults), restore the configured Cargo target directory at setup time and save it at end-of-job so no-op child-branch builds can reuse Cargo fingerprints and outputs. Keys include the runner OS, architecture, resolved toolchain digest, `Cargo.lock` hash, and commit SHA, with fallback limited to the same toolchain and lockfile lineage.

### Current Limits That Must Stay Explicit

The extracted public action must document these current limits honestly:

- the action rehydrates the Soldr root, Cargo home, rustup home, and Soldr-owned zccache artifact cache under the chosen cache/state root
- the action bootstraps `rustup` on demand via `rustup-init` when it is absent, then uses it to install the requested toolchain; at runtime Soldr prefers direct toolchain binaries from `RUSTUP_HOME` / `CARGO_HOME` / `PATH` and only falls back to `rustup which` when the direct probe fails (or when `RUSTUP_TOOLCHAIN` is explicitly set)
- the action exports `ZCCACHE_CACHE_DIR` to the Soldr-owned zccache artifact cache under `SOLDR_CACHE_DIR`
- restored Cargo target directories are fast paths, not freshness overrides; build scripts without precise `cargo:rerun-if-*` inputs can still be dirty on fresh checkouts because source mtimes differ

### Non-Contract Details

These details can change during `v0` while the action is still beta. `v1` should be reserved for the first stable contract with a stronger backward-compatibility promise:

- the exact cache key format
- the default runner-local filesystem path chosen when `cache-dir` is empty
- internal helper script layout or implementation language
- the exact release API calls used to download Soldr

## Release And Tagging Plan

GitHub's current release guidance recommends semantic tags and moving major tags for actions. If immutable releases are enabled, keep the immutable release tags and the moving compatibility tags separate.

References:

- https://docs.github.com/actions/how-tos/create-and-publish-actions/using-immutable-releases-and-tags-to-manage-your-actions-releases
- https://docs.github.com/actions/how-tos/creating-and-publishing-actions/managing-custom-actions

Use this release model for `zackees/setup-soldr`:

1. Validate the action changes in this repository first with the existing smoke path.
2. Copy the release contents into `zackees/setup-soldr` without workflow files.
3. Publish the beta release as an immutable tag such as `v0.1.0`.
4. Move the beta compatibility tag `v0` to the same commit as the latest compatible beta release.
5. Publish `v1.0.0` and move `v1` only when the action is ready for a stable backward-compatible contract.
6. Introduce later major tags only for breaking contract changes such as input removal, output removal, or behavior changes that require workflow edits after `v1`.

## Extraction Checklist

For the public beta release:

1. Create `zackees/setup-soldr` as a public repository.
2. Accept the GitHub Marketplace Developer Agreement for the owning account or organization if it has not already been accepted.
3. Copy [action.yml](../action.yml) and the helper scripts listed above into the public repository.
4. Copy the user-facing setup docs from this repository's `README.md`, `INTEGRATION.md`, and this file into the public repository `README.md`, using `zackees/setup-soldr@v0` as the public action reference.
5. Ensure the public repository contains no workflow files.
6. Create the first beta release tag `v0.1.0`, then move `v0` to that commit.
7. Publish the beta release to GitHub Marketplace from the public repository.

## What This PR Changes

This repo-contained plan is useful because it:

- defines the exact public contract the extracted action should preserve
- distinguishes public contract from current implementation-only escape hatches
- gives a mechanical file-copy plan for extraction
- gives a beta tagging and release model for `@v0`, with `@v1` reserved for the later stable contract
