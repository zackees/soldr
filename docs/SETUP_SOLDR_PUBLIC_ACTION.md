# setup-soldr Public Action Plan

This document is the repo-contained delivery for [issue #137](https://github.com/zackees/soldr/issues/137).

`soldr` cannot publish its current root action to GitHub Marketplace directly. The useful work that can land here is to freeze the intended public `setup-soldr` contract, define the extraction boundary, and document the exact release process that turns the current in-repo action into a separate public action product.

## Why Extraction Is Required

GitHub's current action guidance says public actions are best kept in their own repository, and GitHub Marketplace requires:

- a public repository
- a single root `action.yml` or `action.yaml`
- no workflow files in the published repository

References:

- https://docs.github.com/en/actions/how-tos/sharing-automations/creating-actions/publishing-actions-in-github-marketplace
- https://docs.github.com/actions/how-tos/creating-and-publishing-actions/managing-custom-actions

This repository intentionally contains normal application code and workflow files under `.github/workflows/`, so it is the source repository for the action, not the eventual Marketplace repository.

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

## Stable Public `v1` Contract

The intended public UX is:

```yaml
steps:
  - uses: actions/checkout@v4

  - uses: zackees/setup-soldr@v1
    with:
      version: 0.7.4
      cache: true

  - run: soldr cargo build --locked --release
  - run: soldr cargo test --locked
```

### Supported Inputs

`v1` should treat these inputs as the public contract:

| Input | Meaning |
|---|---|
| `version` | Soldr release tag or version to install. Empty means latest release. |
| `cache` | Restore and save the action-managed cache/state root. |
| `cache-dir` | Override the runner-local cache/state root. |
| `cache-key-suffix` | Optional escape hatch appended to the cache key. |
| `toolchain` | Explicit Rust toolchain channel override. |
| `toolchain-file` | Alternate toolchain file path when `toolchain` is empty. |
| `trust-mode` | Optional `SOLDR_TRUST_MODE` value. |

The current in-repo action also exposes `repo` as an implementation/testing override. That input is not part of the intended public `v1` contract and should not be documented in the extracted public action README.

### Supported Outputs

`v1` should treat these outputs as the public contract:

| Output | Meaning |
|---|---|
| `soldr-path` | Installed Soldr binary path added to `PATH`. |
| `soldr-version` | Installed Soldr version reported by `soldr version --json`. |
| `cache-dir` | Action-managed runner-local cache/state root. |
| `cache-hit` | Whether the action restored an exact cache hit. |
| `toolchain` | Exact Rust toolchain channel configured for the action. |

### Required Behavior

`v1` should preserve these behaviors:

- install exactly one released Soldr binary for the active runner OS and architecture
- provision the normal-path Rust toolchain itself via `rustup`; users should not need a separate toolchain action for the common path
- create and export `SOLDR_CACHE_DIR`, `CARGO_HOME`, and `RUSTUP_HOME`
- put the installed `soldr` binary on `PATH`
- restore and save the action-managed cache/state root when `cache: true`
- export `RUSTUP_TOOLCHAIN` after toolchain installation so later `cargo`, `rustc`, and `soldr cargo ...` steps stay on the same resolved toolchain

### Current Limits That Must Stay Explicit

The extracted public action must document these current limits honestly:

- the action rehydrates the Soldr root, Cargo home, and rustup home under the chosen cache/state root
- Soldr still uses `rustup` under the hood today
- managed `zccache` artifact storage still follows zccache's current supported/default behavior rather than a fully action-controlled custom artifact path

### Non-Contract Details

These details can change inside `v1` without forcing `v2`, as long as the supported inputs, outputs, and behaviors above stay compatible:

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
3. Publish an immutable release tag such as `v1.0.0`.
4. Move the major compatibility tag `v1` to the same commit as the latest compatible `v1.x.y` release.
5. Introduce `v2` only for breaking contract changes such as input removal, output removal, or behavior changes that require workflow edits.

## Extraction Checklist

Before the first public release:

1. Create `zackees/setup-soldr` as a public repository.
2. Accept the GitHub Marketplace Developer Agreement for the owning account or organization if it has not already been accepted.
3. Copy [action.yml](../action.yml) and the helper scripts listed above into the public repository.
4. Copy the user-facing setup docs from this repository's `README.md`, `INTEGRATION.md`, and this file into the public repository `README.md`, but remove any mention of the temporary in-repo `zackees/soldr@<ref>` path.
5. Ensure the public repository contains no workflow files.
6. Create the first release tag `v1.0.0`, then move `v1` to that commit.
7. Publish the release to GitHub Marketplace from the public repository.

## What This PR Changes

This repo-contained plan is useful even before the public repository exists because it:

- defines the exact public contract the extracted action should preserve
- distinguishes public contract from current implementation-only escape hatches
- gives a mechanical file-copy plan for extraction
- gives a stable tagging and release model for `@v1` and later `@v2`
