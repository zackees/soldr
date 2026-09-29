#!/usr/bin/env python3
"""Enforce ci.yml as the repository's only pull-request workflow entry point.

GitHub Actions workflow YAML is inspected structurally: comments and strings
that merely mention an event must not become policy violations.
"""

from __future__ import annotations

import argparse
from pathlib import Path

import yaml

PR_EVENTS = ("pull_request", "pull_request_target")
DEFAULT_WORKFLOWS = Path(".github/workflows")


def workflow_paths(directory: Path) -> list[Path]:
    """Return workflow YAML files in stable order, including both extensions."""

    return sorted([*directory.glob("*.yml"), *directory.glob("*.yaml")])


def closed_only(config: object) -> bool:
    """A `pull_request` trigger limited to `types: [closed]`.

    The invariant exists so that no second PR workflow can become a required
    check. A closed-only trigger runs after the PR is closed, never on an
    open PR's head, so it cannot gate a merge. cache-budget.yml uses it to
    delete a closed PR's Actions-cache entries (zackees/ci.yml#6).
    """
    if not isinstance(config, dict):
        return False
    types = config.get("types")
    if isinstance(types, str):
        types = [types]
    return isinstance(types, list) and types == ["closed"]


def workflow_events(path: Path) -> set[str]:
    """Return PR events declared at the top level of one workflow document."""

    document = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    if not isinstance(document, dict):
        raise ValueError(f"{path}: workflow document must be a mapping")
    # PyYAML's YAML 1.1 resolver turns the unquoted GitHub key `on` into True.
    triggers = document.get("on", document.get(True, {}))
    if isinstance(triggers, dict):
        events = {
            event
            for event, config in triggers.items()
            if not (event == "pull_request" and closed_only(config))
        }
    elif isinstance(triggers, str):
        events = {triggers}
    elif isinstance(triggers, list):
        events = {event for event in triggers if isinstance(event, str)}
    else:
        events = set()
    return set(PR_EVENTS) & events


def check(workflows: Path) -> list[str]:
    """Return violations for PR event declarations outside ci.yml."""

    errors: list[str] = []
    for path in workflow_paths(workflows):
        if path.name == "ci.yml":
            continue
        for event in sorted(workflow_events(path)):
            errors.append(
                f"{path}: declares forbidden {event}; only {workflows / 'ci.yml'} may declare pull-request events"
            )
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workflows", type=Path, default=DEFAULT_WORKFLOWS)
    args = parser.parse_args()
    errors = check(args.workflows)
    if errors:
        print("error: pull-request workflow entry-point invariant failed:")
        print(*[f"  {error}" for error in errors], sep="\n")
        return 1
    print(
        f"check_pr_workflow_triggers: only {args.workflows / 'ci.yml'} declares pull-request events"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
