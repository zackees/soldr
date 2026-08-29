"""Bounded observer for a target-run job that may starve in the Actions queue.

This is deliberately independent of the observed job: a job which ``needs``
the target run cannot report that the target run never received a runner.
"""

from __future__ import annotations

import argparse
from datetime import UTC, datetime
import json
import os
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Sequence


@dataclass(frozen=True)
class Verdict:
    ok: bool
    message: str


def matching_job(jobs: Sequence[dict[str, object]], prefix: str) -> dict[str, object] | None:
    """Return the one matrix-expanded target-run job whose name has ``prefix``."""
    matches = [job for job in jobs if str(job.get("name", "")).startswith(prefix)]
    if len(matches) == 1:
        return matches[0]
    if not matches:
        return None
    raise RuntimeError(f"ambiguous target-run prefix {prefix!r}: {len(matches)} jobs")


def queue_age_seconds(job: dict[str, object], now: datetime) -> float:
    """Measure queue age from Actions' authoritative job creation timestamp."""
    created_at = job.get("created_at")
    if not isinstance(created_at, str):
        raise ValueError("Actions job response is missing created_at")
    try:
        created = datetime.fromisoformat(created_at.replace("Z", "+00:00"))
    except ValueError as error:
        raise ValueError(f"invalid Actions job created_at: {created_at!r}") from error
    if created.tzinfo is None:
        raise ValueError("Actions job created_at has no timezone")
    return max(0.0, (now - created.astimezone(UTC)).total_seconds())


def verdict_for_job(
    job: dict[str, object] | None, *, runner: str, now: datetime, grace_seconds: float
) -> Verdict | None:
    """Classify one poll; ``None`` means keep observing until the deadline."""
    if job is None:
        return None
    name = str(job.get("name", "<unnamed>"))
    status = str(job.get("status", "unknown"))
    conclusion = str(job.get("conclusion") or "")
    queue_age = queue_age_seconds(job, now)
    detail = (
        f"job={name!r} runner={runner!r} status={status!r} conclusion={conclusion!r} "
        f"created_at={job.get('created_at')!r} queue_age={queue_age:.0f}s"
    )
    if status == "in_progress":
        return Verdict(True, f"target-run queue watchdog: started; {detail}")
    if status == "completed":
        if conclusion == "success":
            return Verdict(True, f"target-run queue watchdog: completed successfully; {detail}")
        return Verdict(False, f"target-run queue watchdog: completed unsuccessfully; {detail}")
    if queue_age >= grace_seconds:
        return Verdict(
            False,
            f"target-run queue watchdog: queue starvation exceeded {grace_seconds:.0f}s; {detail}",
        )
    return None


def watch_for_start(
    fetch_jobs: Callable[[], Sequence[dict[str, object]]],
    *,
    job_prefix: str,
    runner: str,
    grace_seconds: float,
    poll_seconds: float,
    now: Callable[[], datetime] = lambda: datetime.now(UTC),
    monotonic: Callable[[], float] = time.monotonic,
    sleep: Callable[[float], None] = time.sleep,
) -> Verdict:
    """Observe queue age, allowing Actions a bounded interval to materialize a job."""
    visibility_started = monotonic()
    while True:
        try:
            job = matching_job(fetch_jobs(), job_prefix)
            if job is None:
                visibility_age = monotonic() - visibility_started
                if visibility_age >= grace_seconds:
                    return Verdict(
                        False,
                        "target-run queue watchdog: Actions API did not materialize the "
                        f"observed job prefix {job_prefix!r} within {grace_seconds:.0f}s; "
                        f"runner={runner!r} visibility_age={visibility_age:.0f}s",
                    )
                sleep(min(poll_seconds, grace_seconds - visibility_age))
                continue
            current = now()
            verdict = verdict_for_job(job, runner=runner, now=current, grace_seconds=grace_seconds)
        except (OSError, ValueError, RuntimeError) as error:
            return Verdict(False, f"target-run queue watchdog: GitHub API error: {error}")
        if verdict is not None:
            return verdict
        sleep(poll_seconds)


def fetch_actions_jobs(repo: str, run_id: str, token: str) -> Sequence[dict[str, object]]:
    request = urllib.request.Request(
        f"https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs?per_page=100",
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        payload = json.load(response)
    jobs = payload.get("jobs")
    if not isinstance(jobs, list):
        raise ValueError("Actions jobs response has no jobs list")
    return jobs


def write_summary(path: str | None, message: str) -> None:
    if path:
        Path(path).write_text(f"### Target-run queue watchdog\n\n{message}\n", encoding="utf-8")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--job-prefix", required=True)
    parser.add_argument("--runner", required=True)
    parser.add_argument("--grace-seconds", type=float, default=900)
    parser.add_argument("--poll-seconds", type=float, default=30)
    parser.add_argument("--github-summary", default=os.environ.get("GITHUB_STEP_SUMMARY"))
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.grace_seconds <= 0 or args.poll_seconds <= 0:
        raise SystemExit("--grace-seconds and --poll-seconds must be positive")
    verdict = watch_for_start(
        lambda: fetch_actions_jobs(args.repo, args.run_id, args.token),
        job_prefix=args.job_prefix,
        runner=args.runner,
        grace_seconds=args.grace_seconds,
        poll_seconds=args.poll_seconds,
    )
    write_summary(args.github_summary, verdict.message)
    print(verdict.message)
    return 0 if verdict.ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
