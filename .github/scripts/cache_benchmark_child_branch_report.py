#!/usr/bin/env python3
"""Generate the parent-to-child cache probe report for issue #168.

This turns the `cache-benchmark-child-branch.yml` workflow's step outputs
into a markdown comment body matching the suggested report shape from
https://github.com/zackees/soldr/issues/168.

Environment variables expected (all optional except where noted):
    COLD_RESULT, COLD_SECONDS
    WARM_RESULT, WARM_SECONDS
    CACHE_HIT, BUILD_CACHE_HIT, TARGET_CACHE_HIT
    SOLDR_VERSION, TOOLCHAIN
    CHILD_BRANCH_INPUT
    OUTPUT_PATH              - where to write the markdown body (required)

    GITHUB_SERVER_URL, GITHUB_REPOSITORY, GITHUB_RUN_ID,
    GITHUB_REF_NAME, GITHUB_SHA, GITHUB_OUTPUT
        - standard GitHub Actions env vars. Missing values are rendered as
          `n/a` so the script stays runnable locally for smoke testing.

The script never raises on missing / malformed input: it degrades to `n/a`
so the comment still posts on partial failures. Exit code is 0 on success.
"""

from __future__ import annotations

import os
import pathlib
import sys
from typing import Optional


def _env(name: str, default: str = "") -> str:
    value = os.environ.get(name)
    if value is None or value == "":
        return default
    return value


def _float_or_none(value: str) -> Optional[float]:
    if value in ("", "n/a"):
        return None
    try:
        return float(value)
    except ValueError:
        return None


def _metric(value: Optional[float], suffix: str = "") -> str:
    return "n/a" if value is None else f"{value:.2f}{suffix}"


def _cache_hit_state(cache_hit: str, build_cache_hit: str) -> str:
    """One-line human summary of the cache-hit triple, for the report."""
    if build_cache_hit == "true":
        return "exact build-cache key match"
    if build_cache_hit == "false" and cache_hit == "true":
        return (
            "setup-state cache exact hit; build-cache restored via restore-keys "
            "fallback or was a cold miss (inspect `build-cache-restore` logs to "
            "distinguish)"
        )
    if build_cache_hit == "false":
        return "no exact build-cache match; inspect raw log for restore-key fallback"
    return "build-cache not enabled or unknown"


def build_report_body(
    cold_result: str,
    cold_seconds: Optional[float],
    warm_result: str,
    warm_seconds: Optional[float],
    cache_hit: str,
    build_cache_hit: str,
    target_cache_hit: str,
    soldr_version: str,
    toolchain: str,
    workflow_run_url: str,
    branch_ref: str,
    commit_sha: str,
) -> str:
    ratio: Optional[float] = None
    saved: Optional[float] = None
    if cold_seconds is not None and warm_seconds not in (None, 0):
        ratio = cold_seconds / warm_seconds  # type: ignore[operator]
        saved = cold_seconds - warm_seconds  # type: ignore[operator]

    subsecond = "yes" if warm_seconds is not None and warm_seconds < 1.0 else "no"
    cache_state = _cache_hit_state(cache_hit, build_cache_hit)

    lines = [
        "### Parent-to-child zccache cache report",
        "",
        f"- workflow run: {workflow_run_url}",
        f"- branch/ref: `{branch_ref}`",
        f"- commit: `{commit_sha}`",
        f"- cold control result: `{cold_result or 'n/a'}`",
        f"- warm setup-soldr result: `{warm_result or 'n/a'}`",
        f"- cold build seconds: `{_metric(cold_seconds)}`",
        f"- warm soldr build seconds: `{_metric(warm_seconds)}`",
        f"- seconds saved: `{_metric(saved)}`",
        f"- speedup vs cold control: `{_metric(ratio, 'x')}`",
        f"- setup cache hit: `{cache_hit or 'n/a'}`",
        f"- zccache build-cache hit: `{build_cache_hit or 'n/a'}`",
        f"- target-cache hit: `{target_cache_hit or 'n/a'}`",
        f"- cache-restore summary: {cache_state}",
        f"- soldr version: `{soldr_version or 'n/a'}`",
        f"- toolchain: `{toolchain or 'n/a'}`",
        f"- sub-second target met: `{subsecond}`",
        "",
        (
            "Note: `build-cache-hit=false` can still indicate a useful restore "
            "via the restore-keys prefix fallback. Inspect the "
            "`build-cache-restore` step's raw log to confirm whether the "
            "parent (main) branch seeded this run or whether the cache was a "
            "cold miss. Re-run this workflow against the same child branch a "
            "second time to force the same-branch warm path."
        ),
    ]
    return "\n".join(lines) + "\n"


def main() -> int:
    output_path = _env("OUTPUT_PATH")
    if not output_path:
        sys.stderr.write("OUTPUT_PATH is required\n")
        return 1

    workflow_run_url = "n/a"
    server = _env("GITHUB_SERVER_URL")
    repo = _env("GITHUB_REPOSITORY")
    run_id = _env("GITHUB_RUN_ID")
    if server and repo and run_id:
        workflow_run_url = f"{server}/{repo}/actions/runs/{run_id}"

    child_branch_input = _env("CHILD_BRANCH_INPUT")
    branch_ref = child_branch_input or _env("GITHUB_REF_NAME", "n/a")
    commit_sha = _env("GITHUB_SHA", "n/a")

    body = build_report_body(
        cold_result=_env("COLD_RESULT"),
        cold_seconds=_float_or_none(_env("COLD_SECONDS")),
        warm_result=_env("WARM_RESULT"),
        warm_seconds=_float_or_none(_env("WARM_SECONDS")),
        cache_hit=_env("CACHE_HIT"),
        build_cache_hit=_env("BUILD_CACHE_HIT"),
        target_cache_hit=_env("TARGET_CACHE_HIT"),
        soldr_version=_env("SOLDR_VERSION"),
        toolchain=_env("TOOLCHAIN"),
        workflow_run_url=workflow_run_url,
        branch_ref=branch_ref,
        commit_sha=commit_sha,
    )

    path = pathlib.Path(output_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")

    github_output = os.environ.get("GITHUB_OUTPUT")
    if github_output:
        with open(github_output, "a", encoding="utf-8") as fh:
            fh.write(f"body_path={path}\n")

    sys.stdout.write(body)
    return 0


if __name__ == "__main__":
    sys.exit(main())
