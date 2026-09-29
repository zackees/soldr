#!/usr/bin/env python3
"""Every Actions-cache save reachable from a pull request carries `pr-<N>`.

zackees/ci.yml#6 ("PRs save one small delta ... keyed by PR number"): a cache
saved in PR context is scoped to that PR by GitHub and restorable by nothing
else, so the cache janitor (`check_cache_budget.py`, run by ci-pre.yml) must
be able to tell which PR it belongs to and delete it once the PR closes. The
ref (`refs/pull/<N>/merge`) says so today, but the key is what survives into
every listing, report and restore-key prefix, so the key carries it too.

## The one expression

Every PR-reachable workflow defines, at workflow level::

    env:
      PR_CACHE_TAG: ${{ github.event_name == 'pull_request' && format('-pr-{0}', github.event.pull_request.number) || '' }}

and every save-capable cache step puts `${{ env.PR_CACHE_TAG }}` in its key
(or `cache-suffix` / `cache-key-suffix`). Outside a pull request the tag is
empty, so main, schedule and non-PR pushes keep their current keys.

## What counts as save-capable in PR context

A step in a workflow reachable from a `pull_request` trigger (ci.yml, and
every reusable workflow it calls, transitively) that uses:

* `actions/cache` or `actions/cache/save`, unless the step's `if:` excludes
  pull requests (`refs/heads/main`, or `github.event_name != 'pull_request'`);
* `astral-sh/setup-uv`, unless `enable-cache: false` or its `if:` excludes PRs;
* `Swatinem/rust-cache`, unless `save-if` names `refs/heads/main`.

`zackees/setup-soldr` with `save-cache: auto` already skips saves on pull
requests and needs no tag. `actions/cache/restore` never saves.

Usage:
    python .github/scripts/check_pr_cache_keys.py [--workflows DIR]
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

import yaml

TAG_NAME = "PR_CACHE_TAG"
TAG_REFERENCE = "env.PR_CACHE_TAG"
TAG_EXPRESSION = (
    "${{ github.event_name == 'pull_request' && "
    "format('-pr-{0}', github.event.pull_request.number) || '' }}"
)
DEFAULT_WORKFLOWS = Path(".github/workflows")
LOCAL_CALL = re.compile(r"^\./\.github/workflows/(?P<name>[\w.-]+\.ya?ml)$")
PR_EXCLUSIONS = (
    "refs/heads/main",
    "github.event_name != 'pull_request'",
    'github.event_name != "pull_request"',
)


def load(path: Path) -> dict:
    document = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    return document if isinstance(document, dict) else {}


def triggers(document: dict) -> set[str]:
    raw = document.get("on", document.get(True, {}))
    if isinstance(raw, dict):
        return {
            event
            for event, config in raw.items()
            if not (
                event == "pull_request"
                and isinstance(config, dict)
                and config.get("types") == ["closed"]
            )
        }
    if isinstance(raw, str):
        return {raw}
    if isinstance(raw, list):
        return {event for event in raw if isinstance(event, str)}
    return set()


def pr_reachable(workflows: Path) -> set[str]:
    """Workflow file names that can run in an open PR's context."""
    documents = {
        path.name: load(path)
        for path in [*workflows.glob("*.yml"), *workflows.glob("*.yaml")]
    }
    reachable = {
        name
        for name, document in documents.items()
        if triggers(document) & {"pull_request", "pull_request_target"}
    }
    frontier = list(reachable)
    while frontier:
        document = documents.get(frontier.pop(), {})
        for job in (document.get("jobs") or {}).values():
            if not isinstance(job, dict):
                continue
            match = LOCAL_CALL.match(str(job.get("uses") or ""))
            if match and match["name"] not in reachable:
                reachable.add(match["name"])
                frontier.append(match["name"])
    return reachable


def excludes_prs(condition: object) -> bool:
    text = str(condition or "")
    return any(marker in text for marker in PR_EXCLUSIONS)


def untagged_saves(document: dict) -> list[str]:
    """`job/step` labels of PR-context saves whose key lacks the tag."""
    found: list[str] = []
    for job_id, job in (document.get("jobs") or {}).items():
        if not isinstance(job, dict):
            continue
        if excludes_prs(job.get("if")):
            continue
        for index, step in enumerate(job.get("steps") or []):
            if not isinstance(step, dict) or excludes_prs(step.get("if")):
                continue
            uses = str(step.get("uses") or "")
            inputs = step.get("with") or {}
            text = " ".join(str(value) for value in inputs.values())
            label = f"{job_id}/{step.get('name') or step.get('id') or index}"
            saves = False
            if uses.startswith("actions/cache@") or uses.startswith(
                "actions/cache/save@"
            ):
                saves = True
            elif uses.startswith("astral-sh/setup-uv@"):
                saves = str(inputs.get("enable-cache", "auto")).lower() != "false"
            elif uses.startswith("Swatinem/rust-cache@"):
                saves = "refs/heads/main" not in str(inputs.get("save-if", ""))
            if saves and TAG_REFERENCE not in text:
                found.append(label)
    return found


def check(workflows: Path) -> list[str]:
    errors: list[str] = []
    for name in sorted(pr_reachable(workflows)):
        path = workflows / name
        document = load(path)
        missing = untagged_saves(document)
        if not missing:
            continue
        env = document.get("env") or {}
        if env.get(TAG_NAME) != TAG_EXPRESSION:
            errors.append(
                f"{path}: define workflow-level env {TAG_NAME}: {TAG_EXPRESSION}"
            )
        for label in missing:
            errors.append(
                f"{path}: {label} can save a cache in PR context without "
                f"${{{{ {TAG_REFERENCE} }}}} in its key"
            )
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workflows", type=Path, default=DEFAULT_WORKFLOWS)
    args = parser.parse_args(argv)
    errors = check(args.workflows)
    if errors:
        print("error: PR-context cache saves must carry the pr-<N> key tag:")
        print(*[f"  {error}" for error in errors], sep="\n")
        return 1
    print("check_pr_cache_keys: every PR-context cache save carries pr-<N>")
    return 0


if __name__ == "__main__":
    sys.exit(main())
