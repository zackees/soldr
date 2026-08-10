# Ralph Loop

This loop is for changes that must be validated on GitHub remote runners, not on the local machine.

## Rules

- Remote GitHub runners are the source of truth.
- Do not rely on local runtime or process-behavior tests.
- Every meaningful branch update must be pushed to the remote branch.
- Merge only after the PR is green on GitHub.

## Loop

1. Submit to remote.

   - Make the branch changes.
   - Run safe local quality checks only.
   - Commit the changes.
   - Push the branch to the PR head.

   Example:

   ```bash
   git add -A
   git commit -m "fix: <reason>"
   git push origin HEAD:<pr-branch>
   ```

2. Wait for builder status.

   - Poll the PR checks on GitHub.
   - If checks are queued or in progress, wait.
   - If no checks appear, assume the branch/PR needs another remote update and push again after confirming the branch is correct.

   Example:

   ```bash
   gh pr checks <pr-number> --repo <owner/repo>
   gh pr view <pr-number> --repo <owner/repo> --json statusCheckRollup
   ```

3. If not building, update branch/PR and push again.

   - Confirm the local branch is still the PR branch.
   - Confirm the PR head matches the local HEAD.
   - If the branch or PR metadata changed, push the current HEAD again.
   - If needed, add a small diagnostic-only commit and push so GitHub re-runs the remote builders.

4. If building, wait for completion.

   - Do not guess from partial output.
   - Wait until every required remote check is in a terminal state.

5. If any check fails, repair and repeat.

   - Pull the failing job logs and artifacts from GitHub.
   - Use the remote failure output as the defect report.
   - Improve diagnostics if the failure is still opaque.
   - Fix the code or test placement.
   - Re-run safe local quality checks.
   - Commit and push again.

   Safe local checks:

   ```bash
   uv run ruff check <files>
   uv run python -m py_compile <files>
   git diff --check
   ```

6. If all required checks are green, merge to `main`.

   - Verify the PR head is the commit that went green.
   - Merge using the repo's merge policy.
   - Prefer a non-interactive GitHub CLI merge.

   Example:

   ```bash
   gh pr merge <pr-number> --repo <owner/repo> --squash --delete-branch
   ```

7. Always push to remote after branch updates.

   - Never leave the authoritative state only on the local machine.
   - The remote branch and PR must always reflect the latest fix attempt.

## Pseudocode

```text
loop:
  change branch
  run safe local quality checks
  commit
  push to remote PR branch

  wait for GitHub checks

  if no builders started:
    refresh branch/PR state
    push again
    continue

  if any required check failed:
    inspect remote logs/artifacts
    improve diagnostics if needed
    fix the concrete failure
    continue

  if all required checks passed:
    merge PR to main
    stop
```
