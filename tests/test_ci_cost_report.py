"""Offline contract tests for the GitHub Actions cost report."""

import importlib.util
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / ".github/scripts/ci_cost_report.py"


def load_report():
    spec = importlib.util.spec_from_file_location("ci_cost_report", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(
    run_id,
    sha,
    *,
    event="push",
    attempts=1,
    workflow=10,
    created_at="2026-01-01T00:00:00Z",
    display_title=None,
):
    return {
        "id": run_id,
        "head_sha": sha,
        "event": event,
        "workflow_id": workflow,
        "name": "CI",
        "run_attempt": attempts,
        "status": "completed",
        "conclusion": "success",
        "html_url": f"https://example.test/runs/{run_id}",
        "created_at": created_at,
        "display_title": display_title
        or (f"CI full {sha}" if event == "workflow_dispatch" else "CI"),
    }


def job(job_id, start, end, conclusion="success", labels=None):
    return {
        "id": job_id,
        "name": "build",
        "started_at": start,
        "completed_at": end,
        "conclusion": conclusion,
        "labels": labels or ["ubuntu-latest"],
    }


class FakeAPI:  # pylint: disable=too-few-public-methods
    def __init__(self, runs, jobs):
        self.runs, self.jobs, self.calls = runs, jobs, []

    def get(self, endpoint, params=None):
        self.calls.append((endpoint, params))
        page = int((params or {}).get("page", 1))
        if endpoint.endswith("/actions/runs"):
            return {"workflow_runs": self.runs.get(page, [])}
        if "/attempts/" not in endpoint:
            run_id = int(endpoint.rsplit("/", 1)[1])
            return next(
                item
                for batch in self.runs.values()
                for item in batch
                if item["id"] == run_id
            )
        run_id = int(endpoint.split("/runs/")[1].split("/")[0])
        attempt = int(endpoint.split("/attempts/")[1].split("/")[0])
        return {"jobs": self.jobs.get((run_id, attempt, page), [])}


class CostReportTests(unittest.TestCase):
    def test_repeated_label_runs_on_one_sha_need_an_anchor(self):
        m = load_report()
        api = FakeAPI(
            {
                1: [
                    run(1, "a", event="pull_request"),
                    run(
                        2,
                        "a",
                        event="pull_request",
                        created_at="2026-01-01T00:10:00Z",
                    ),
                ]
            },
            {
                (1, 1, 1): [job(11, "2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z")],
            },
        )
        with self.assertRaisesRegex(ValueError, "anchor"):
            m.collect_sha(api, "o/r", "a", "pull_request")
        report = m.collect_sha(api, "o/r", "a", "pull_request", anchor_run_id=1)
        self.assertEqual([run["id"] for run in report["included_runs"]], [1])
        self.assertEqual(report["excluded_runs"][0]["reason"], "outside-anchor-window")

    def test_one_second_github_clock_skew_is_recorded_not_lost(self):
        m = load_report()
        api = FakeAPI(
            {1: [run(1, "a")], 2: []},
            {
                (1, 1, 1): [job(11, "2026-01-01T00:01:00Z", "2026-01-01T00:00:59Z")],
                (1, 1, 2): [],
            },
        )
        report = m.collect_sha(api, "o/r", "a", "push")
        self.assertEqual(report["runner_minutes"], 0)
        self.assertEqual(report["jobs"][0]["timestamp_skew_seconds"], 1)
        with self.assertRaises(ValueError):
            m.seconds("2026-01-01T00:01:00Z", "2026-01-01T00:00:55Z")

    def test_paginates_all_workflows_jobs_and_attempts_and_explains_exclusions(self):
        m = load_report()
        api = FakeAPI(
            {
                1: [run(1, "a", attempts=2), run(2, "a", event="workflow_dispatch")]
                + [run(n, "other") for n in range(100, 198)],
                2: [run(3, "b"), run(4, "a", workflow=20)],
                3: [],
            },
            {
                (1, 1, 1): [
                    job(11, "2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z", "failure")
                ]
                + [job(n, None, None) for n in range(100, 199)],
                (1, 1, 2): [],
                (1, 2, 1): [job(12, "2026-01-01T00:00:00Z", "2026-01-01T00:02:00Z")],
                (1, 2, 2): [],
                (4, 1, 1): [
                    job(41, "2026-01-01T00:00:00Z", "2026-01-01T00:03:00Z", "cancelled")
                ],
                (4, 1, 2): [],
            },
        )
        report = m.collect_sha(api, "o/r", "a", "push")
        self.assertEqual(report["runner_minutes"], 6)
        self.assertEqual([r["id"] for r in report["included_runs"]], [1, 4])
        self.assertEqual(
            [(r["id"], r["reason"]) for r in report["excluded_runs"]],
            [(2, "event=workflow_dispatch")],
        )
        self.assertEqual(
            len([c for c in api.calls if c[0].endswith("/actions/runs")]), 2
        )
        self.assertTrue(
            any("/attempts/1/jobs" in c[0] and c[1]["page"] == 2 for c in api.calls)
        )
        self.assertTrue(any("/attempts/1/jobs" in c[0] for c in api.calls))
        self.assertTrue(any("/attempts/2/jobs" in c[0] for c in api.calls))

    def test_explicit_cohorts_median_range_ratio_and_threshold(self):
        m = load_report()
        baseline = [
            {"sha": "a", "runner_minutes": 100},
            {"sha": "b", "runner_minutes": 120},
        ]
        post = [{"sha": "c", "runner_minutes": 10}, {"sha": "d", "runner_minutes": 15}]
        result = m.compare(baseline, post)
        self.assertEqual(result["baseline"]["median_runner_minutes"], 110)
        self.assertEqual(result["baseline"]["range_runner_minutes"], [100, 120])
        self.assertEqual(result["post"]["median_runner_minutes"], 12.5)
        self.assertEqual(result["median_ratio"], 12.5 / 110)
        self.assertTrue(result["meets_12_5_percent_target"])
        with self.assertRaises(ValueError):
            m.compare(baseline, [post[0], {"sha": "a", "runner_minutes": 1}])

    def test_same_sha_label_waves_can_be_compared_by_distinct_anchors(self):
        m = load_report()
        result = m.compare(
            [{"sha": "a", "anchor_run_id": 1, "runner_minutes": 100}],
            [{"sha": "a", "anchor_run_id": 2, "runner_minutes": 10}],
        )
        self.assertEqual(result["median_ratio"], 0.1)
        self.assertEqual(len(result["workflow_ids_by_sample"]), 2)

    def test_explicit_full_dispatch_can_be_a_push_cost_control(self):
        m = load_report()
        api = FakeAPI(
            {1: [run(1, "a" * 40, event="workflow_dispatch")], 2: []},
            {(1, 1, 1): [job(11, "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z")]},
        )
        baseline = m.collect_sha(
            api, "o/r", "a" * 40, "workflow_dispatch", anchor_run_id=1
        )
        post = {
            "sha": "a" * 40,
            "anchor_run_id": 2,
            "event": "push",
            "runner_minutes": 5,
        }
        with self.assertRaisesRegex(ValueError, "same event"):
            m.compare([baseline], [post])
        result = m.compare([baseline], [post], allow_cross_event=True)
        self.assertEqual(result["median_ratio"], 5 / 60)
        self.assertTrue(result["cross_event_control"])

    def test_dispatch_cost_uses_candidate_title_not_workflow_ref_sha(self):
        m = load_report()
        api = FakeAPI(
            {
                1: [
                    run(
                        7,
                        "b" * 40,
                        event="workflow_dispatch",
                        display_title=f"CI full {'a' * 40}",
                    )
                ]
            },
            {(7, 1, 1): [job(71, "2026-01-01T00:00:00Z", "2026-01-01T00:02:00Z")]},
        )
        report = m.collect_sha(
            api, "o/r", "a" * 40, "workflow_dispatch", anchor_run_id=7
        )
        self.assertEqual(report["runner_minutes"], 2)
        self.assertEqual(report["included_runs"][0]["id"], 7)
        self.assertEqual(report["included_runs"][0]["head_sha"], "b" * 40)
        with self.assertRaisesRegex(ValueError, "anchor"):
            m.collect_sha(api, "o/r", "a" * 40, "workflow_dispatch")
        with self.assertRaisesRegex(ValueError, "candidate"):
            m.collect_sha(api, "o/r", "c" * 40, "workflow_dispatch", anchor_run_id=7)

    def test_weighting_unknown_labels_are_disclosed(self):
        m = load_report()
        api = FakeAPI(
            {1: [run(1, "a")], 2: []},
            {
                (1, 1, 1): [
                    job(
                        11,
                        "2026-01-01T00:00:00Z",
                        "2026-01-01T00:02:00Z",
                        labels=["ubuntu-latest"],
                    ),
                    job(
                        12,
                        "2026-01-01T00:00:00Z",
                        "2026-01-01T00:01:00Z",
                        labels=["self-hosted", "large"],
                    ),
                ],
                (1, 1, 2): [],
            },
        )
        report = m.collect_sha(
            api, "o/r", "a", "push", weights={"ubuntu-latest": 4}
        )
        self.assertEqual(report["weighted_runner_minutes_known"], 8)
        self.assertEqual(report["unknown_weight_job_ids"], [12])


if __name__ == "__main__":
    unittest.main()
