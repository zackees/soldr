#!/usr/bin/env python3
"""Read-only merged-candidate prerequisite for default-ref fleet dispatch.

Run from an independently reviewed helper revision before candidate code. This
is not a publication approval or protection against candidate-controlled YAML.
API documents passed to validate_candidate are trusted caller inputs; the CLI
fetches them from GitHub using the authoritative repository/default branch.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import urllib.parse
import urllib.request
from pathlib import Path

SHA = re.compile(r"[0-9a-f]{40}\Z")
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")


def validate_candidate(
    repository_document: dict,
    branch_document: dict,
    comparison: dict,
    pulls: list,
    *,
    repository: str,
    candidate_sha: str,
    invocation_ref: str,
) -> dict:
    """Bind a merged PR head or merge commit to current default ancestry."""
    if not REPOSITORY.fullmatch(repository) or not SHA.fullmatch(candidate_sha):
        raise ValueError("authoritative repository and exact lowercase SHA required")
    default = repository_document.get("default_branch")
    if (
        repository_document.get("full_name") != repository
        or not isinstance(default, str)
        or not default
        or invocation_ref != "refs/heads/" + default
        or branch_document.get("name") != default
    ):
        raise ValueError("dispatch must use the authoritative default branch")
    default_sha = branch_document.get("commit", {}).get("sha")
    if not isinstance(default_sha, str) or not SHA.fullmatch(default_sha):
        raise ValueError("authoritative default commit is missing")
    if (
        comparison.get("base_commit", {}).get("sha") != default_sha
        or comparison.get("merge_base_commit", {}).get("sha") != candidate_sha
        or comparison.get("status") not in {"identical", "behind"}
    ):
        raise ValueError("candidate is not an ancestor of the observed default commit")
    merged = []
    for pull in pulls:
        if not isinstance(pull, dict):
            raise ValueError("invalid associated pull request response")
        number = pull.get("number")
        base = pull.get("base", {})
        if (
            pull.get("merged_at")
            and isinstance(number, int)
            and not isinstance(number, bool)
            and number > 0
            and base.get("ref") == default
            and base.get("repo", {}).get("full_name") == repository
            and candidate_sha
            in {pull.get("merge_commit_sha"), pull.get("head", {}).get("sha")}
        ):
            merged.append(number)
    if not merged:
        raise ValueError("candidate lacks an associated merged default-branch PR")
    return {
        "schema": "fleet-ci-candidate/v1",
        "repository": repository,
        "candidate_sha": candidate_sha,
        "default_branch": default,
        "authoritative_default_sha": default_sha,
        "merged_pull_requests": sorted(set(merged)),
        "success": True,
    }


def github_get(repository: str, path: str, token: str):
    request = urllib.request.Request(
        f"https://api.github.com/repos/{repository}" + ("/" + path if path else ""),
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": "Bearer " + token,
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate-sha", required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args(argv)
    repo = os.environ["GITHUB_REPOSITORY"]
    if not REPOSITORY.fullmatch(repo) or not SHA.fullmatch(args.candidate_sha):
        raise ValueError("authoritative repository and exact lowercase SHA required")
    token = os.environ["GITHUB_TOKEN"]
    metadata = github_get(repo, "", token)
    default = metadata["default_branch"]
    # Fail before further reads or any candidate execution on an untrusted ref.
    if os.environ["GITHUB_REF"] != "refs/heads/" + default:
        raise ValueError("dispatch must use the authoritative default branch")
    branch = github_get(repo, "branches/" + urllib.parse.quote(default, safe=""), token)
    default_sha = branch["commit"]["sha"]
    comparison = github_get(
        repo, f"compare/{default_sha}...{args.candidate_sha}", token
    )
    pulls = []
    for page in range(1, 1001):
        rows = github_get(
            repo, f"commits/{args.candidate_sha}/pulls?per_page=100&page={page}", token
        )
        if not isinstance(rows, list):
            raise ValueError("invalid associated pull request response")
        pulls.extend(rows)
        if len(rows) < 100:
            break
    else:
        raise ValueError("associated pull request pagination exhausted")
    report = validate_candidate(
        metadata,
        branch,
        comparison,
        pulls,
        repository=repo,
        candidate_sha=args.candidate_sha,
        invocation_ref=os.environ["GITHUB_REF"],
    )
    args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
