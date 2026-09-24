#!/usr/bin/env python3
"""Report all GitHub Actions runner time for exact SHAs and a routine event.

Example: uv run --no-project python .github/scripts/ci_cost_report.py \
  --repo zackees/soldr --event push --baseline-sha SHA1 --baseline-sha SHA2 \
  --post-sha SHA3 --post-sha SHA4

For same-SHA PR label transitions, repeat each SHA with a distinct matching
``--baseline-anchor-run-id`` or ``--post-anchor-run-id``. Anchor IDs are paired
with SHA arguments in their respective order.

To compare a routine main push with a full-equivalent manual dispatch on the
same candidate SHA, use ``--baseline-event workflow_dispatch`` and
``--post-event push`` with explicit anchors for each wave. A dispatch anchor is
mandatory: GitHub's run ``head_sha`` names the workflow ref, which can differ
from the candidate checked out by CI. The dispatch must have the workflow's
``CI full <candidate_sha>`` run title. Cross-event controls are reported as
such, never disguised as pre/post samples of one event.

Only ``gh api`` read calls are made. Supply anchor run IDs when a SHA was used
for multiple PR label transitions; the report refuses to merge event waves
more than two minutes apart without them. The output is JSON so the inputs,
included and excluded runs, attempts, jobs, and arithmetic remain inspectable. Wall time
is an estimate of runner time, not GitHub's billed-minute ledger. Optional
``--weights`` accepts a JSON object mapping a job runner label to a vCPU or
billing multiplier; unmapped jobs are excluded from weighted sums and listed.
"""

import argparse
import json
import re
import statistics
import subprocess
from datetime import datetime

PAGE_SIZE = 100
EVENT_WINDOW_SECONDS = 120
EVENTS = ("push", "pull_request", "workflow_dispatch")


class GhAPI:
    def get(self, endpoint, params=None):
        cmd = ["gh", "api", "--method", "GET", endpoint]
        for key, value in (params or {}).items():
            cmd += ["-f", f"{key}={value}"]
        return json.loads(subprocess.check_output(cmd, text=True))


def pages(api, endpoint, key, params=None):
    """Fetch all pages, including the final page after a full page of 100."""
    page = 1
    while True:
        data = api.get(
            endpoint, {**(params or {}), "per_page": PAGE_SIZE, "page": page}
        )
        items = data[key]
        yield from items
        if len(items) < PAGE_SIZE:
            break
        page += 1


def parse_time(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def seconds(start, end):
    if not start or not end:
        return None, 0

    elapsed = (parse_time(end) - parse_time(start)).total_seconds()
    if elapsed < 0:
        # GitHub's second-resolution job timestamps occasionally arrive one
        # second out of order. Retain the job and disclose the skew instead of
        # discarding an otherwise valid event-wide cost sample.
        if elapsed >= -2:
            return 0, -elapsed
        raise ValueError(f"negative job duration: {start} to {end}")
    return elapsed, 0


def collect_sha(api, repo, sha, event, *, weights=None, anchor_run_id=None):
    if event not in EVENTS:
        raise ValueError(f"event must be one of {EVENTS}")
    if not sha:
        raise ValueError("empty SHA")
    if weights is not None and any(value <= 0 for value in weights.values()):
        raise ValueError("weights must be positive")
    if event == "workflow_dispatch":
        if anchor_run_id is None:
            raise ValueError("workflow_dispatch requires an anchor run ID")
        if not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
            raise ValueError("dispatch candidate SHA must be 40 hex characters")
        run = api.get(f"repos/{repo}/actions/runs/{anchor_run_id}")
        if (
            run.get("event") != event
            or run.get("name") != "CI"
            or run.get("display_title", "").lower() != f"CI full {sha}".lower()
        ):
            raise ValueError(
                f"anchor run {anchor_run_id} is not CI full for candidate {sha}"
            )
        runs = [run]
        event_runs = runs
    else:
        endpoint = f"repos/{repo}/actions/runs"
        runs = list(pages(api, endpoint, "workflow_runs", {"head_sha": sha}))
        event_runs = [
            run for run in runs if run["head_sha"] == sha and run["event"] == event
        ]
    anchor_time = None
    if anchor_run_id is not None:
        anchors = [run for run in event_runs if run["id"] == anchor_run_id]
        if len(anchors) != 1:
            raise ValueError(
                f"anchor run {anchor_run_id} is not a {event} run at {sha}"
            )
        anchor_time = parse_time(anchors[0]["created_at"])
    elif len(event_runs) > 1:
        times = [parse_time(run["created_at"]) for run in event_runs]
        if (max(times) - min(times)).total_seconds() > EVENT_WINDOW_SECONDS:
            raise ValueError(
                f"multiple {event} event waves at {sha}; supply an anchor run ID"
            )
    included, excluded, jobs_out = [], [], []
    total_seconds = weighted_seconds = 0
    unknown_weights = []
    for run in sorted(runs, key=lambda item: item["id"]):
        if event != "workflow_dispatch" and run["head_sha"] != sha:
            continue  # Defensive against an API filter regression.
        info = {
            key: run.get(key)
            for key in (
                "id",
                "workflow_id",
                "name",
                "display_title",
                "event",
                "head_sha",
                "status",
                "conclusion",
                "html_url",
                "created_at",
                "run_attempt",
            )
        }
        if run["event"] != event:
            excluded.append({**info, "reason": f"event={run['event']}"})
            continue
        if (
            anchor_time is not None
            and abs((parse_time(run["created_at"]) - anchor_time).total_seconds())
            > EVENT_WINDOW_SECONDS
        ):
            excluded.append({**info, "reason": "outside-anchor-window"})
            continue
        if run["status"] != "completed":
            raise ValueError(
                f"run {run['id']} is not completed; rerun after it finishes"
            )
        included.append(info)
        for attempt in range(1, run["run_attempt"] + 1):
            job_endpoint = (
                f"repos/{repo}/actions/runs/{run['id']}/attempts/{attempt}/jobs"
            )
            for job in pages(api, job_endpoint, "jobs"):
                duration, timestamp_skew = seconds(
                    job.get("started_at"), job.get("completed_at")
                )
                if duration is None and job.get("started_at"):
                    raise ValueError(f"job {job['id']} has no completion time")
                duration = duration or 0
                total_seconds += duration
                labels = job.get("labels") or []
                matched = [label for label in labels if weights and label in weights]
                if weights is not None:
                    if len(matched) == 1:
                        weighted_seconds += duration * weights[matched[0]]
                    elif duration:
                        unknown_weights.append(job["id"])
                jobs_out.append(
                    {
                        "run_id": run["id"],
                        "attempt": attempt,
                        "id": job["id"],
                        "name": job.get("name"),
                        "conclusion": job.get("conclusion"),
                        "labels": labels,
                        "seconds": duration,
                        "timestamp_skew_seconds": timestamp_skew,
                        "weight_label": matched[0] if len(matched) == 1 else None,
                    }
                )
    if not included:
        raise ValueError(f"no completed {event} workflow runs for {sha}")
    return {
        "sha": sha,
        "event": event,
        "anchor_run_id": anchor_run_id,
        "event_window_seconds": EVENT_WINDOW_SECONDS,
        "runner_minutes": total_seconds / 60,
        "weighted_runner_minutes_known": (
            weighted_seconds / 60 if weights is not None else None
        ),
        "unknown_weight_job_ids": unknown_weights,
        "included_runs": included,
        "excluded_runs": excluded,
        "jobs": jobs_out,
    }


def summarize(reports):
    values = [report["runner_minutes"] for report in reports]
    if not values:
        raise ValueError("a cohort needs at least one SHA")
    return {
        "sha_count": len(values),
        "median_runner_minutes": statistics.median(values),
        "range_runner_minutes": [min(values), max(values)],
        "samples": reports,
    }


def compare(baseline, post, allow_cross_event=False):
    identities = [
        (report["sha"], report.get("anchor_run_id"), report.get("event"))
        for report in baseline + post
    ]
    if len(set(identities)) != len(identities):
        raise ValueError("baseline and post samples must be unique and disjoint")
    if len(baseline) != len(post):
        raise ValueError("baseline and post cohorts need the same number of samples")
    events = {report.get("event") for report in baseline + post if report.get("event")}
    if len(events) > 1 and not allow_cross_event:
        raise ValueError("baseline and post cohorts must use the same event")
    before, after = summarize(baseline), summarize(post)
    if before["median_runner_minutes"] == 0:
        raise ValueError("baseline median is zero; ratio is undefined")
    ratio = after["median_runner_minutes"] / before["median_runner_minutes"]
    inventories = {
        f"{report['sha']}@{report.get('anchor_run_id') or 'all'}:{report.get('event') or 'unspecified'}": sorted(
            {run["workflow_id"] for run in report.get("included_runs", [])}
        )
        for report in baseline + post
    }
    return {
        "baseline": before,
        "post": after,
        "median_ratio": ratio,
        "cross_event_control": len(events) > 1,
        "workflow_ids_by_sample": inventories,
        "workflow_inventory_matched": len({tuple(ids) for ids in inventories.values()})
        <= 1,
        "meets_12_5_percent_target": ratio <= 0.125,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="OWNER/REPO")
    parser.add_argument("--event", choices=EVENTS)
    parser.add_argument("--baseline-event", choices=EVENTS)
    parser.add_argument("--post-event", choices=EVENTS)
    parser.add_argument("--baseline-sha", action="append", required=True)
    parser.add_argument("--post-sha", action="append", required=True)
    parser.add_argument("--baseline-anchor-run-id", type=int, action="append")
    parser.add_argument("--post-anchor-run-id", type=int, action="append")
    parser.add_argument("--weights", help="JSON label-to-positive-multiplier file")
    args = parser.parse_args(argv)
    if args.event and (args.baseline_event or args.post_event):
        parser.error("use --event or separate baseline/post events, not both")
    baseline_event = args.event or args.baseline_event
    post_event = args.event or args.post_event
    if not baseline_event or not post_event:
        parser.error("supply --event or both --baseline-event and --post-event")
    weights = None
    if args.weights:
        with open(args.weights, encoding="utf-8") as stream:
            weights = json.load(stream)
        if not isinstance(weights, dict) or not all(
            isinstance(key, str) and isinstance(value, (int, float)) and value > 0
            for key, value in weights.items()
        ):
            parser.error("--weights must map labels to positive numeric multipliers")

    def cohort_samples(shas, anchors, label):
        if anchors and len(anchors) != len(shas):
            parser.error(f"{label} anchors must match {label} SHA count")
        return list(zip(shas, anchors or [None] * len(shas), strict=True))

    baseline_samples = cohort_samples(
        args.baseline_sha, args.baseline_anchor_run_id, "baseline"
    )
    post_samples = cohort_samples(args.post_sha, args.post_anchor_run_id, "post")
    api = GhAPI()
    reports = {}
    for event, samples in (
        (baseline_event, baseline_samples),
        (post_event, post_samples),
    ):
        for sha, anchor in samples:
            key = (sha, anchor, event)
            if key not in reports:
                reports[key] = collect_sha(
                    api, args.repo, sha, event, weights=weights, anchor_run_id=anchor
                )
    result = compare(
        [reports[sample[0], sample[1], baseline_event] for sample in baseline_samples],
        [reports[sample[0], sample[1], post_event] for sample in post_samples],
        allow_cross_event=baseline_event != post_event,
    )
    result.update(
        {
            "repo": args.repo,
            "baseline_event": baseline_event,
            "post_event": post_event,
            "weights": weights,
            "measurement": "sum of completed job wall seconds over every attempt; "
            "not GitHub billed minutes; missing runner labels leave weighted cost unknown",
        }
    )
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
