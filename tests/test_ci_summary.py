"""Always-running, fail-closed merge summary (setup-soldr#523)."""

import json
from pathlib import Path

import pytest
import yaml
from conftest import load_script_module

ROOT = Path(__file__).parents[1]
SUMMARY = load_script_module(ROOT / ".github/scripts/ci_summary.py", "ci_summary")
SHA = "a" * 40


def adapter():
    return {
        "schema_version": 1,
        "minimal_jobs": ["lint", "host"],
        "docs_jobs": ["docs-lint"],
        "test_jobs": ["extra-test"],
    }


def full_contract():
    return {
        "schema_version": 1,
        "required_jobs": ["lint", "host", "platform-smoke"],
        "targets": [
            {"triple": "board", "ci": {"build_job": "build", "run_job": "run"}}
        ],
    }


def check(needs, **overrides):
    inputs = {
        "mode": "full",
        "docs_only": False,
        "event_name": "pull_request",
        "author_permission": "read",
        "expected_sha": SHA,
        "selected_sha": SHA,
        **overrides,
    }
    return SUMMARY.summary_failures(adapter(), full_contract(), needs, **inputs)


def passing():
    return {
        job: {"result": "success"}
        for job in SUMMARY.required_jobs(adapter(), full_contract(), "full", False)
    }


@pytest.mark.parametrize(
    "state", ["missing", "skipped", "cancelled", "neutral", "failure"]
)
def test_green_aggregate_cannot_hide_one_unsuccessful_platform(state):
    needs = passing()
    if state == "missing":
        del needs["run"]
    else:
        needs["run"] = {"result": state}
    assert f"run: {state}" in check(needs)


def test_full_pass_binds_candidate_and_includes_the_test_tier():
    assert check(passing()) == []
    assert check(passing(), selected_sha="b" * 40)
    needs = passing()
    del needs["extra-test"]
    assert "extra-test: missing" in check(needs)


@pytest.mark.parametrize("permission", ["read", "triage", "none", "unknown", ""])
def test_external_author_cannot_downgrade_even_with_all_minimal_jobs_green(permission):
    assert "external or unresolved author requires full CI" in check(
        passing(), mode="minimal", author_permission=permission
    )


def test_trusted_minimal_does_not_require_skipped_full_jobs():
    needs = {
        job: {"result": "success"}
        for job in ["ci-mode", "path-selection", "lint", "host"]
    }
    assert check(needs, mode="minimal", author_permission="write") == []
    assert check(needs, mode="test", author_permission="write") == [
        "extra-test: missing"
    ]
    assert check(
        needs, mode="minimal", author_permission="write", event_name="workflow_dispatch"
    )


def test_docs_gate_requires_successful_path_selection_and_docs_lint():
    needs = {
        job: {"result": "success"} for job in ["ci-mode", "path-selection", "docs-lint"]
    }
    assert check(needs, mode="minimal", docs_only=True, author_permission="admin") == []
    needs["path-selection"] = {"result": "failure"}
    assert check(needs, mode="minimal", docs_only=True, author_permission="admin")
    assert check(needs, docs_only=True)


def test_unknown_mode_fails_clearly():
    with pytest.raises(ValueError, match="unknown CI mode"):
        check(passing(), mode="")


def test_build_only_target_cannot_pass_summary():
    contract = full_contract()
    contract["targets"][0]["ci"]["run_job"] = None
    assert SUMMARY.summary_failures(
        adapter(),
        contract,
        passing(),
        mode="full",
        docs_only=False,
        event_name="pull_request",
        author_permission="read",
        expected_sha=SHA,
        selected_sha=SHA,
    ) == ["board: no full-CI execution job"]


def test_cli_failure_report_preserves_candidate_identity(tmp_path, monkeypatch):
    adapter_path = tmp_path / "adapter.json"
    manifest_path = tmp_path / "full.json"
    report_path = tmp_path / "summary.json"
    adapter_path.write_text(json.dumps(adapter()))
    manifest_path.write_text(json.dumps(full_contract()))
    for key, value in {
        "CI_NEEDS_JSON": json.dumps(passing()),
        "CI_MODE": "full",
        "DOCS_ONLY": "false",
        "AUTHOR_PERMISSION": "read",
        "GITHUB_EVENT_NAME": "pull_request",
        "EXPECTED_SHA": SHA,
        "SELECTED_SHA": "b" * 40,
    }.items():
        monkeypatch.setenv(key, value)
    monkeypatch.setattr(
        "sys.argv",
        [
            "ci_summary.py",
            "--adapter",
            str(adapter_path),
            "--full-contract",
            str(manifest_path),
            "--report",
            str(report_path),
        ],
    )
    assert SUMMARY.main() == 1
    report = json.loads(report_path.read_text())
    assert report["schema"] == "fleet-ci-summary/v1"
    assert report["expected_sha"] == SHA
    assert not report["success"]
    assert report["failures"]


def test_merge_summary_runs_even_when_selector_or_required_cells_fail():
    jobs = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())["jobs"]
    summary = jobs.get("ci-summary")
    assert summary, "a skipped Full coverage job is not a merge summary"
    assert summary["name"] == "CI summary"
    assert summary["if"] == "${{ always() }}"
    assert set(jobs["full-coverage"]["needs"]) <= set(summary["needs"])
    assert {"lint-docs", "path-selection", "full-coverage"} <= set(summary["needs"])


def test_extended_tier_overrides_docs_only_skip_for_its_required_host():
    jobs = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())["jobs"]
    for name in ("lint", "build-linux-x64"):
        assert "needs.ci-mode.outputs.mode != 'minimal'" in jobs[name]["if"]
    assert "needs.ci-mode.outputs.mode == 'minimal'" in jobs["lint-docs"]["if"]
