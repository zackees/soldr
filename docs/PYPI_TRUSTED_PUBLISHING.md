# PyPI Trusted Publishing

This document describes the repo-side and owner-side steps needed to publish `soldr` wheels to PyPI using GitHub Actions OIDC Trusted Publishing.

It is intentionally PyPI-only. crates.io publication is not part of the current release direction.

## Current State

As of April 19, 2026:

- `.github/workflows/release-auto.yml` is the only release workflow
- that workflow always builds the hardened `soldr` wheel set as part of a real release
- the workflow publishes those wheels through `pypa/gh-action-pypi-publish` in a dedicated OIDC job
- the existing PyPI project `soldr` already exists

The next secure PyPI release should come from the same validated workflow that created the GitHub Release.

## Why This Uses Trusted Publishing

PyPI Trusted Publishing avoids long-lived API tokens.

The workflow receives a short-lived upload credential from PyPI using the GitHub Actions OIDC identity for a specific repository, workflow file, and optional environment.

For `soldr`, the intended trusted identity is:

- owner: `zackees`
- repository: `soldr`
- workflow: `.github/workflows/release-auto.yml`
- environment: `release`

Using the existing `release` environment keeps the PyPI publish step behind the same final approval boundary as GitHub Release publication.

## Owner Setup On PyPI

These steps must be performed in the PyPI web UI by a maintainer of the `soldr` project.

1. Open the `soldr` project on PyPI.
2. Click `Manage`.
3. Open the `Publishing` page in the project sidebar.
4. Add a new GitHub Actions publisher with:
   - repository owner: `zackees`
   - repository name: `soldr`
   - workflow filename: `.github/workflows/release-auto.yml`
   - environment name: `release`

The environment field is optional in PyPI, but it should be filled in here because the repo already uses `release` as the publish-time credential boundary.

## Repo-Side Workflow Behavior

The release workflow now does this whenever a reviewed version bump lands on `main`:

1. Derives the release version from `[workspace.package]` in `Cargo.toml`.
2. Verifies that the version changed from the previous `main` commit.
3. Builds hardened wheels on the supported platform runners.
4. Smoke-tests the built wheel on each runner by installing it and running `soldr --version`.
5. Publishes the GitHub Release.
6. Publishes the wheel set to PyPI from a dedicated Linux job with `id-token: write`.

No source distribution is published in this path. The current design is wheel-only because the project is prioritizing hardened binary distribution rather than source release through package registries.

## Expected Manual Stops

The repo-side work can be completed without additional secrets.

The remaining manual dependencies are:

- the PyPI trusted publisher must be registered
- the `release` environment must approve the final publication jobs

Until the publisher is registered, the PyPI publish job will not be able to mint a trusted upload token.
