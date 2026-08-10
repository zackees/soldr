# Releasing

Releases are driven by the **Auto Release** workflow
(`.github/workflows/auto-release.yml`).

## Trigger

The workflow fires on any of:

- A push to `main` that bumps `pyproject.toml` `project.version`.
- A push of a `vX.Y.Z` or `X.Y.Z` tag.
- A manual `gh workflow run auto-release.yml` dispatch.

The `detect-bump` job short-circuits if there's no version change on a
branch push, or if a GitHub Release already exists for the version, so
re-triggering is safe.

## One-time prerequisites

These need to exist on the repo *before* the first real release runs:

- **PyPI trusted publisher** registered on the running-process PyPI
  project. Set it to:
  - Owner: `zackees`
  - Repository: `running-process`
  - Workflow: `auto-release.yml`
  - Environment: `pypi`
- **GitHub environment `pypi`** with `id-token: write` allowed. The
  workflow's `publish-pypi` job opts into it via
  `environment: pypi`.
- **Repo secret `CARGO_REGISTRY_TOKEN`** containing a crates.io API
  token with publish scope on the
  `running-process-{proto,core,client,py}` crates. Without it the
  `publish-crates` job hard-fails before doing anything destructive.

## Cutting a release

1. Bump the version in **every** manifest. `ci/version_check.py`
   enforces these stay in lockstep — running it locally is the fastest
   sanity check:
   ```
   uv run --module ci.version_check
   ```
   The current list (keep this in sync with `MANIFESTS` in that file):
   - `pyproject.toml` — `project.version`
   - `Cargo.toml` — `workspace.package.version`
   - `src/running_process/__init__.py` — `__version__` literal
   - `crates/running-process-py/Cargo.toml` internal path-dep version pin
     (`{ path = "../running-process", version = "X.Y.Z" }`)
   - `crates/running-process/Cargo.toml` pin on `running-process-probe`
   - `crates/running-process-probe-daemon/Cargo.toml` pins on
     `running-process` and `running-process-probe`

   Every path dependency of a *published* crate needs a version, including
   optional ones behind a feature. Missing one is not caught until
   `cargo publish` refuses the package.
   - `Cargo.lock` (regenerate via `soldr cargo check --workspace`)
   - `uv.lock` (regenerate via `uv lock`)
2. Open a PR with just the bump. Once it merges to `main`, the
   workflow detects the version change and fires.
3. Watch the workflow run. The job graph is:
   ```
   detect-bump -> preflight -> { build-wheels-* x6 , build-binaries (matrix) }
                            -> publish-pypi
                            -> publish-crates
                            -> publish-release
   ```
   `preflight` queries PyPI and crates.io to set `pypi_complete` /
   `crates_complete`. Either flag short-circuits its publish job, so
   re-runs after partial failures are idempotent.

## What gets published

- **PyPI**: `running-process` wheels for linux x86/arm, macOS x86/arm,
  Windows x86/arm, plus the sdist (built on the linux-x86 runner).
  Published via `pypa/gh-action-pypi-publish@release/v1` with
  `skip-existing: true` (OIDC, no static token).
- **crates.io**, in dep order (probe crates added per #651):
  1. `running-process-probe` — the probe protocol types and the
     all-thread snapshot. `running-process`'s `probe` feature depends on
     it, so it has to exist on the index first or `running-process`
     cannot resolve at all.
  2. `running-process` (depends on the previous one)
  3. `running-process-probe-daemon` — `rpprobed` and `rpprobe`. Depends
     on both of the above.
  4. `running-process-py` (depends on `running-process`)

  This order is a dependency constraint, not a preference: publishing out
  of it fails to resolve rather than merely being untidy. Keep it in sync
  with **both** lists in `auto-release.yml` — `rust_crates` in the
  preflight job and `PUBLISH_ORDER` in `publish-crates`.
  `cargo publish` already blocks on the sparse-index appearance before
  returning — the index is what the next `cargo publish` reads to
  resolve internal path-deps — so the loop trusts cargo's own wait and
  does **not** add a second poll against the JSON API at
  `/api/v1/crates/$name/$version` (that endpoint propagates separately
  and can lag the index by minutes).
- **GitHub Release**: wheels, sdist, standalone `runpm` and
  `running-process-daemon` archives (`.tar.gz` for unix, `.zip` for
  Windows), `install.sh`, `install.ps1`, and `SHA256SUMS`.

## Reproducible builds

Set `RUNNING_PROCESS_REPRODUCIBLE=1` to normalize timestamps, absolute
build paths, and incremental-compilation noise so two builds of the
same commit are byte-identical. The seam, the build-twice/compare-SHA256
verification recipe, and the Linux CI spot-check are documented in
[reproducible-builds.md](reproducible-builds.md).

## Failure modes & recovery

| Symptom | What it means | Fix |
| --- | --- | --- |
| `Trusted publishing exchange failure ... invalid-publisher` | PyPI doesn't have a trusted publisher row matching `repo:zackees/running-process:environment:pypi` + `workflow:auto-release.yml`. | Add/correct the row in PyPI project settings. Re-run the workflow. |
| `ERROR: CARGO_REGISTRY_TOKEN secret is not set` | Repo secret missing. | Add the secret with publish scope on the four publishable crates and re-run. |
| `ci.version_check` ERROR | One manifest didn't get bumped. | Run `uv run --module ci.version_check` locally; fix the file it names; ensure `Cargo.lock` + `uv.lock` regenerated. |
| Partial crates.io publish (some crates uploaded, later ones not) | Almost always a transient cargo / network issue; the workflow is idempotent. | Re-run the same workflow run. `preflight` skips crates already on crates.io and `publish-crates` skips them in its inner loop. |
| `running-process` was published with a wire-format regression | The 3.2.x→3.3.0 wire change taught us this can happen (proto types now live inside `running-process` itself post-#165). | `cargo yank --version X.Y.Z -p running-process` to block new resolutions. Yank doesn't delete; existing lockfiles keep working. Then bump and publish a corrected version. |

## Per-platform dev workflows

The `linux-x86-build.yml`, `windows-x86-build.yml`, etc. workflows
still run on every push to `main` in dev mode. They are independent
of the release workflow and do not gate it.
