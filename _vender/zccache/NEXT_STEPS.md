# Next Steps

Here is the exact checklist.

1. Get the repo changes onto GitHub.

Commit and push the release automation changes, ideally to `main`:

- [action.yml](/C:/Users/niteris/dev/zccache/action.yml)
- [.github/workflows/release.yml](/C:/Users/niteris/dev/zccache/.github/workflows/release.yml)
- [.github/workflows/build.yml](/C:/Users/niteris/dev/zccache/.github/workflows/build.yml)
- [.github/actions/build-target/action.yml](/C:/Users/niteris/dev/zccache/.github/actions/build-target/action.yml)
- [ci/release_workflow.py](/C:/Users/niteris/dev/zccache/ci/release_workflow.py)

2. Create the GitHub environment for PyPI.

In `zackees/zccache`:
`Settings` -> `Environments` -> `New environment`

Create:
`pypi`

You do not need to add any secret there for PyPI if we use Trusted Publishing.

Optional: add required reviewers if you want manual approval before publish.

3. Register GitHub as a Trusted Publisher on PyPI.

In PyPI, open the `zccache` project:
`https://pypi.org/manage/project/zccache/`

Then:
`Publishing` -> `Add a publisher` -> `GitHub Actions`

Use these values:

- Owner: `zackees`
- Repository name: `zccache`
- Workflow:
  If PyPI asks for the workflow filename, use `release.yml`
  If it asks for the workflow path, use `.github/workflows/release.yml`
- Environment name: `pypi`

That is the PyPI setup. No PyPI token is needed.

4. Create the crates.io token.

Go to:
`https://crates.io/me`

Create a new API token.
Copy it immediately, because crates.io only shows it once.

5. Add the crates.io token to GitHub.

In `zackees/zccache`:
`Settings` -> `Secrets and variables` -> `Actions` -> `New repository secret`

Create:

- Name: `CARGO_REGISTRY_TOKEN`
- Value: the crates.io token you just made

6. Re-run the `1.2.15` release with the fixed workflow.

Because `1.2.15` already exists as a tag and the old workflow failed, do this:

- Go to `Actions`
- Open workflow `Release`
- Click `Run workflow`
- Branch: `main`
- Input `tag`: `1.2.15`

That will:

- build release artifacts
- build wheels
- publish to PyPI
- publish crates to crates.io
- create the GitHub release

7. Publish the action to GitHub Marketplace.

This part is still manual in GitHub’s UI.

After the workflow creates the GitHub release:

- open the new release page
- check `Publish this action to the GitHub Marketplace`
- choose categories
- publish

8. Verify the result.

Check:

- GitHub Releases shows `1.2.15` as latest
- PyPI shows `zccache 1.2.15`
- crates.io shows the `1.2.15` crates
- the repo page no longer looks stuck on `1.2.8`

## GitHub follow-up after creating the crates.io token

1. Add the crates.io token as a repo secret.

In `zackees/zccache`:
`Settings` -> `Secrets and variables` -> `Actions` -> `New repository secret`

Set:

- Name: `CARGO_REGISTRY_TOKEN`
- Secret: paste the crates.io token

2. Create the PyPI environment.

In `zackees/zccache`:
`Settings` -> `Environments` -> `New environment`

Create:

- `pypi`

You do not need to add a PyPI token there.

3. Push the release workflow changes if you have not already.

The repo must include the updated:

- `.github/workflows/release.yml`
- `.github/workflows/build.yml`
- `.github/actions/build-target/action.yml`
- `action.yml`
- `ci/release_workflow.py`

4. Set up PyPI Trusted Publishing.

On PyPI for project `zccache`, add a GitHub Actions trusted publisher with:

- Owner: `zackees`
- Repository: `zccache`
- Workflow: `release.yml` or `.github/workflows/release.yml`
- Environment: `pypi`

5. Re-run the release workflow for the existing tag.

In GitHub:
`Actions` -> `Release` -> `Run workflow`

Use:

- Branch: `main`
- Tag input: `1.2.15`

6. After the workflow succeeds, publish to Marketplace manually.

Open the generated GitHub release and check:

- `Publish this action to the GitHub Marketplace`
