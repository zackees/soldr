---
name: pr-wait-fast
description: Wait on a PR's CI checks and return on the FIRST failure or when all pass, instead of `gh pr checks --watch` which waits for the slowest lane. Optionally cancel every still-running run on first failure so the whole fan-out fails fast.
---

# pr-wait-fast

Use this whenever you need to block on a pull request's CI. Never use
`gh pr checks --watch` or `gh run watch`: both wait for every lane to finish,
and in this repo the macOS and Windows target-runs routinely queue 15-80
minutes behind a Lint failure that was decided in minute two.

## Run

```bash
uv run --no-project python .claude/skills/pr-wait-fast/pr_wait_fast.py <pr> [--cancel]
```

`<pr>` is a number, branch name, or URL. Flags:

| Flag | Default | Meaning |
|---|---|---|
| `--cancel` | off | On the first failing check, cancel every queued or in-progress workflow run for the PR's head commit. Use this when the user asked for the whole run to fail fast. |
| `--interval` | 20 | Poll period in seconds. |
| `--timeout` | 5400 | Give up (exit 2) after this many seconds with checks still pending. |
| `--grace` | 90 | How long to tolerate "no checks reported yet" right after a push. |
| `--repo` | current | `owner/name` when not inside the repo checkout. |

## Exit codes

| Code | Meaning | What to do next |
|---|---|---|
| 0 | every check passed or was skipped | proceed (merge, report green) |
| 1 | at least one check failed; names and job links are printed | open the first link with `gh run view <run> --job <job> --log-failed`, fix, push, run this again |
| 2 | deadline hit or no checks ever appeared | report pending lanes; do not claim green |
| 3 | `gh` error (auth, unknown PR) | fix the invocation |

## Behaviour

- Polls `gh pr checks --json` and prints a one-line bucket summary only when
  it changes, so a long wait produces a short transcript.
- Returns the moment any check lands in the `fail` bucket. Pending checks are
  not waited for.
- Returns 0 only when no check is pending and none failed.
- With `--cancel`, resolves the PR head SHA and runs `gh run cancel` on every
  run for that commit that is still queued or running. Cancelled runs show as
  `cancelled` on the PR, which is expected; the printed failure is the real one.

## Triage after a failure

1. Take the job link the script printed. Fetch the failed step with
   `gh run view <run-id> --job <job-id> --log-failed` (or `gh api
   repos/<owner>/<repo>/actions/jobs/<job-id>` for the failing step name while
   the run is still in progress).
2. Reproduce locally before editing: Python lanes run
   `uv run --no-project --with pytest --with pyyaml python -m pytest tests/ .github/scripts/ -q`;
   Rust lanes go through `soldr cargo nextest run`.
3. Push the fix to the PR branch and run this skill again.

## Run in the background

For long waits, launch with `run_in_background` on the Bash tool and let the
completion notification re-invoke you; do not poll with `sleep` loops.
