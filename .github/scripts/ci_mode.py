#!/usr/bin/env python3
"""Select routine or complete CI coverage and pin the source commit."""

from __future__ import annotations

import argparse
import json
import os
import re
import time
import urllib.error
import urllib.parse
import urllib.request
from email.utils import parsedate_to_datetime
from pathlib import Path

SELECTOR_SCHEMA = "fleet-ci-mode/v1"


def label_aliases_from_adapter(adapter: dict[str, object]) -> dict[str, str]:
    """Map reviewed legacy controls onto literal fleet labels, never downgrade."""
    schema = adapter.get("schema_version")
    aliases = adapter.get("label_aliases")
    if (
        not isinstance(schema, int)
        or isinstance(schema, bool)
        or schema != 1
        or not isinstance(aliases, dict)
    ):
        raise ValueError(
            "selector adapter requires integer schema_version 1 and label_aliases"
        )
    for alias, target in aliases.items():
        if (
            not isinstance(alias, str)
            or not alias.strip()
            or alias in {"ci-test", "ci-full"}
            or not isinstance(target, str)
            or target not in {"ci-test", "ci-full"}
        ):
            raise ValueError(
                "label aliases must map legacy names to ci-test or ci-full"
            )
    return aliases


def select_mode(
    event_name: str,
    event: dict[str, object],
    candidate_sha: str,
    *,
    author_permission: str | None = None,
    label_aliases: dict[str, str] | None = None,
) -> tuple[str, str]:
    aliases = label_aliases_from_adapter(
        {"schema_version": 1, "label_aliases": label_aliases or {}}
    )
    if event_name == "workflow_dispatch":
        if not re.fullmatch(r"[0-9a-fA-F]{40}", candidate_sha):
            raise ValueError(
                "workflow_dispatch requires a full 40-character candidate SHA"
            )
        return "full", candidate_sha.lower()
    if event_name == "merge_group":
        group = event.get("merge_group")
        sha = group.get("head_sha") if isinstance(group, dict) else None
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
            raise ValueError("merge_group head SHA is missing or invalid")
        return "full", sha.lower()
    if event_name == "pull_request":
        pr = event.get("pull_request")
        if not isinstance(pr, dict):
            raise ValueError("pull_request payload is missing")
        head = pr.get("head")
        sha = head.get("sha") if isinstance(head, dict) else None
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
            raise ValueError("pull_request head SHA is missing or invalid")
        labels = pr.get("labels")
        names = (
            {
                aliases.get(label["name"], label["name"])
                for label in labels
                if isinstance(label, dict) and isinstance(label.get("name"), str)
            }
            if isinstance(labels, list)
            else set()
        )
        mode = (
            "full"
            if author_permission not in {"write", "maintain", "admin"}
            or "ci-full" in names
            else "test" if "ci-test" in names else "minimal"
        )
        return mode, sha.lower()
    if event_name == "push":
        sha = event.get("after")
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
            raise ValueError("push commit SHA is missing or invalid")
        return "minimal", sha.lower()
    raise ValueError(f"unsupported CI event: {event_name}")


def permission_from_response(
    data: dict, *, login: str, response_date: str, now: float
) -> str | None:
    """Bind a fresh permission response to the queried author, never association."""
    try:
        age = now - parsedate_to_datetime(response_date).timestamp()
    except (ValueError, TypeError, OverflowError):
        return None
    user = data.get("user")
    if (
        not 0 <= age <= 60
        or not isinstance(user, dict)
        or str(user.get("login", "")).casefold() != login.casefold()
    ):
        return None
    permission = data.get("permission")
    return (
        permission
        if isinstance(permission, str)
        and permission in {"admin", "write", "read", "none"}
        else None
    )


def lookup_author_permission(event: dict, repository: str, token: str) -> str | None:
    """Read effective base-repo permission anew on every run, including reruns."""
    pr = event.get("pull_request")
    if not isinstance(pr, dict):
        return None
    base = pr.get("base")
    base = base.get("repo") if isinstance(base, dict) else None
    user = pr.get("user")
    login = user.get("login") if isinstance(user, dict) else None
    if (
        not token
        or not re.fullmatch(r"[\w.-]+/[\w.-]+", repository)
        or not isinstance(base, dict)
        or base.get("full_name") != repository
        or not isinstance(login, str)
        or not login
    ):
        return None
    author = urllib.parse.quote(login, safe="")
    request = urllib.request.Request(
        f"https://api.github.com/repos/{repository}/collaborators/{author}/permission",
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "Cache-Control": "no-cache",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            data = json.load(response)
            if not isinstance(data, dict):
                return None
            return permission_from_response(
                data,
                login=login,
                response_date=response.headers.get("Date", ""),
                now=time.time(),
            )
    except (urllib.error.URLError, OSError, ValueError):
        return None


def verify_checkout(expected_sha: str, checked_out_sha: str) -> None:
    if checked_out_sha.lower() != expected_sha:
        raise ValueError(f"checked out {checked_out_sha}, expected {expected_sha}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event-name", default=os.environ.get("GITHUB_EVENT_NAME", ""))
    parser.add_argument("--event-path", default=os.environ.get("GITHUB_EVENT_PATH", ""))
    parser.add_argument("--candidate-sha", default="")
    parser.add_argument("--adapter", type=Path)
    parser.add_argument("--checked-out-sha", required=True)
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT", ""))
    args = parser.parse_args()
    event = json.loads(Path(args.event_path).read_text(encoding="utf-8"))
    permission = (
        lookup_author_permission(
            event,
            os.environ.get("GITHUB_REPOSITORY", ""),
            os.environ.get("GITHUB_TOKEN", ""),
        )
        if args.event_name == "pull_request"
        else None
    )
    adapter = (
        json.loads(args.adapter.read_text(encoding="utf-8")) if args.adapter else None
    )
    if args.adapter and not isinstance(adapter, dict):
        raise ValueError("selector adapter must be an object")
    mode, sha = select_mode(
        args.event_name,
        event,
        args.candidate_sha,
        author_permission=permission,
        label_aliases=(
            label_aliases_from_adapter(adapter) if adapter is not None else None
        ),
    )
    verify_checkout(sha, args.checked_out_sha)
    if not args.github_output:
        raise ValueError("GITHUB_OUTPUT is required")
    with Path(args.github_output).open("a", encoding="utf-8") as output:
        output.write(f"mode={mode}\ncheckout_sha={sha}\n")
        output.write(f"selector_schema={SELECTOR_SCHEMA}\n")
        output.write(f"author_permission={permission or 'unknown'}\n")
    print(
        f"CI mode: {mode}; checked out SHA: {sha}; "
        f"author permission: {permission or 'unknown'}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
