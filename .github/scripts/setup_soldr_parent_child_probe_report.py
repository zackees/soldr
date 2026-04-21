#!/usr/bin/env python3
"""Render and optionally post the setup-soldr parent-to-child cache probe report."""

from __future__ import annotations

import json
import os
import urllib.request
from pathlib import Path
from typing import Any


def _read_float(value: Any) -> float | None:
    if value in ("", None):
        return None
    return float(value)


def _format_seconds(value: float | None) -> str:
    return "n/a" if value is None else f"{value:.2f}"


def _format_ratio(value: float | None) -> str:
    return "n/a" if value is None else f"{value:.2f}x"


def _hit_value(value: Any) -> str:
    if value in ("", None):
        return "n/a"
    return str(value).lower()


def _speedup(cold: float | None, warm: float | None) -> float | None:
    if cold is None or warm is None or warm <= 0:
        return None
    return cold / warm


def _saved_seconds(cold: float | None, warm: float | None) -> float | None:
    if cold is None or warm is None:
        return None
    return cold - warm


def _dominant_remaining_cost(payload: dict[str, Any], warm: float | None) -> str:
    if warm is not None and warm <= 1.0:
        return "none observed; sub-second target met"
    if _hit_value(payload.get("target_cache_hit")) != "true":
        return "target cache was not an exact hit; inspect restore-key fallback logs"
    if _hit_value(payload.get("build_cache_hit")) != "true":
        return "zccache build-cache was not an exact hit; inspect build-cache restore logs"

    status_lines = "\n".join(payload.get("zccache_status_lines") or [])
    if "0 cached" in status_lines or "Hit rate: 0.0%" in status_lines:
        return "zccache misses despite restored caches"
    return "Cargo metadata/checking, cache restore overhead, or non-cacheable work"


def build_report(payload: dict[str, Any]) -> str:
    cold = _read_float(payload.get("cold_seconds"))
    warm = _read_float(payload.get("warm_seconds"))
    saved = _saved_seconds(cold, warm)
    ratio = _speedup(cold, warm)
    subsecond = warm is not None and warm <= 1.0
    status_lines = payload.get("zccache_status_lines") or []
    status_block = "\n".join(status_lines).strip() or "n/a"

    lines = [
        "### Parent-to-child zccache cache report",
        "",
        f"- workflow run: {payload.get('run_url', 'n/a')}",
        f"- branch/ref: `{payload.get('ref_name', 'n/a')}`",
        f"- commit: `{payload.get('sha', 'n/a')}`",
        f"- cold control result: `{payload.get('cold_result', 'n/a')}`",
        f"- warm setup-soldr result: `{payload.get('warm_result', 'n/a')}`",
        f"- cold build seconds: `{_format_seconds(cold)}`",
        f"- warm soldr build seconds: `{_format_seconds(warm)}`",
        f"- seconds saved: `{_format_seconds(saved)}`",
        f"- speedup vs cold control: `{_format_ratio(ratio)}`",
        f"- setup cache hit: `{_hit_value(payload.get('setup_cache_hit'))}`",
        f"- zccache build-cache hit: `{_hit_value(payload.get('build_cache_hit'))}`",
        f"- target-cache hit: `{_hit_value(payload.get('target_cache_hit'))}`",
        f"- soldr version: `{payload.get('soldr_version') or 'n/a'}`",
        f"- toolchain: `{payload.get('toolchain') or 'n/a'}`",
        f"- zccache artifact cache dir: `{payload.get('zccache_artifact_cache_dir') or 'n/a'}`",
        f"- sub-second target met: `{'yes' if subsecond else 'no'}`",
        f"- dominant remaining cost: `{_dominant_remaining_cost(payload, warm)}`",
        "",
        "zccache status lines:",
        "",
        "```text",
        status_block,
        "```",
        "",
        "Note: `build-cache-hit=false` or `target-cache-hit=false` may still mean a useful restore-key fallback. Inspect the raw cache step logs to distinguish fallback restores from cold misses.",
    ]
    return "\n".join(lines) + "\n"


def post_issue_comment(repo: str, issue_number: str, token: str, body: str) -> None:
    url = f"https://api.github.com/repos/{repo}/issues/{issue_number}/comments"
    request = urllib.request.Request(
        url,
        data=json.dumps({"body": body}).encode("utf-8"),
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "User-Agent": "setup-soldr-parent-child-probe",
            "X-GitHub-Api-Version": "2022-11-28",
        },
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        if response.status >= 300:
            raise RuntimeError(f"GitHub issue comment failed with HTTP {response.status}")


def main() -> None:
    input_path = Path(os.environ["PROBE_INPUT_JSON"])
    output_path = Path(os.environ["PROBE_OUTPUT_MD"])
    payload = json.loads(input_path.read_text(encoding="utf-8"))
    report = build_report(payload)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(report, encoding="utf-8")

    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_path:
        with Path(summary_path).open("a", encoding="utf-8") as handle:
            handle.write(report)

    if os.environ.get("PROBE_POST_COMMENT", "").lower() == "true":
        post_issue_comment(
            repo=os.environ["GITHUB_REPOSITORY"],
            issue_number=os.environ["PROBE_ISSUE_NUMBER"],
            token=os.environ["GITHUB_TOKEN"],
            body=report,
        )


if __name__ == "__main__":
    main()
