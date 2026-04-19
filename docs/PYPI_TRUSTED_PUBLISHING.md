# PyPI Trusted Publishing

This document describes the repo-side and owner-side steps needed to publish `soldr` wheels to PyPI using GitHub Actions OIDC Trusted Publishing.

It is intentionally PyPI-only. crates.io publication is not part of the current `0.5.x` release direction.

## Current State

As of April 14, 2026:

- the release workflow can build hardened `soldr` wheels when `publish_pypi=true`
- the workflow can publish those wheels through `pypa/gh-action-pypi-publish` in a dedicated OIDC job
- the existing PyPI project `soldr` already exists
- the current public PyPI `0.1.0` file details say `Uploaded using Trusted Publishing? No`

The next secure PyPI release should replace that stale packaging path with the real wheel set built from the validated release workflow.

## Why This Uses Trusted Publishing

PyPI Trusted Publishing avoids long-lived API tokens.

The workflow receives a short-lived upload credential from PyPI using the GitHub Actions OIDC identity for a specific repository, workflow file, and optional environment.

For `soldr`, the intended trusted identity is:

- owner: `zackees`
- repository: `soldr`
- workflow: `.github/workflows/release.yml`
- environment: `release`

Using the existing `release` environment keeps the PyPI publish step behind the same manual approval gate as the immutable GitHub release path.

The unattended path in `.github/workflows/release-auto.yml` is a second trusted identity:

- owner: `zackees`
- repository: `soldr`
- workflow: `.github/workflows/release-auto.yml`
- environment: leave blank

That second publisher is required if autonomous releases should also publish to PyPI, because PyPI binds Trusted Publishers to a specific workflow filename.

## Owner Setup On PyPI

These steps must be performed in the PyPI web UI by a maintainer of the `soldr` project.

1. Open the `soldr` project on PyPI.
2. Click `Manage`.
3. Open the `Publishing` page in the project sidebar.
4. Add a new GitHub Actions publisher with:
   - repository owner: `zackees`
   - repository name: `soldr`
   - workflow filename: `.github/workflows/release.yml`
   - environment name: `release`

The environment field is optional in PyPI, but it should be filled in here because the repo already uses `release` as the human approval boundary.

If autonomous releases are enabled, add a second publisher with:

- repository owner: `zackees`
- repository name: `soldr`
- workflow filename: `.github/workflows/release-auto.yml`
- environment name: leave empty

## Repo-Side Workflow Inputs

The release workflow supports these PyPI-related inputs:

- `publish_pypi=true`
- `pypi_repository_url=...` for alternate endpoints such as TestPyPI

If `publish_pypi=false`, the workflow keeps its GitHub-release-only behavior.

If `publish_pypi=true`, the workflow:

1. Builds hardened wheels on the supported platform runners.
2. Smoke-tests the built wheel on each runner by installing it and running `soldr --version`.
3. Publishes the GitHub Release.
4. Publishes the wheel set to PyPI from a dedicated Linux job with `id-token: write`.

No source distribution is published in this path. The current design is wheel-only because the project is prioritizing hardened binary distribution rather than source release through package registries.

The unattended workflow supports the same release surfaces but derives the version from `Cargo.toml` and the target commit from the `main` branch head:

```bash
gh workflow run release-auto.yml --ref main
```

Dry-run rehearsal remains available:

```bash
gh workflow run release-auto.yml \
  --ref main \
  -f dry_run=true
```

## Recommended Rehearsal

Before enabling real PyPI publishing, rehearse against TestPyPI.

Recommended command:

```bash
gh workflow run release.yml \
  --ref release \
  -f version=v0.5.0-rc1 \
  -f commit_sha=<40-char-sha> \
  -f dry_run=false \
  -f publish_pypi=true \
  -f pypi_repository_url=https://test.pypi.org/legacy/
```

This uses a real publish path instead of `dry_run=true`, because the OIDC publisher exchange only happens in the real publish job.

## Expected Manual Stop

The repo-side work can be completed without additional secrets.

If the PyPI trusted publisher has not been registered yet, that PyPI-side publisher registration is the remaining manual stop. Until that is done, the PyPI publish job will not be able to mint a trusted upload token.
