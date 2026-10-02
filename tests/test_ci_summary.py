"""Always-running, fail-closed merge summary (setup-soldr#523)."""

import ast
import json
from pathlib import Path

import pytest
import yaml
from conftest import load_script_module

ROOT = Path(__file__).parents[1]
SUMMARY = load_script_module(ROOT / ".github/scripts/ci_summary.py", "ci_summary")
SHA = "a" * 40


@pytest.mark.parametrize(
    "mode,trusted,docs,expected",
    [
        ("minimal", "false", "false", (False, True, True)),
        ("minimal", "false", "true", (True, False, False)),
        ("minimal", "true", "false", (True, False, False)),
        ("minimal", "true", "true", (True, False, False)),
        ("test", "false", "false", (False, True, True)),
        ("test", "false", "true", (False, True, True)),
        ("test", "true", "false", (False, True, True)),
        ("test", "true", "true", (False, True, True)),
        ("full", "false", "false", (False, True, True)),
        ("full", "false", "true", (False, True, True)),
        ("full", "true", "false", (False, True, True)),
        ("full", "true", "true", (False, True, True)),
    ],
)
def test_workflow_attestation_never_skips_expanded_tiers(mode, trusted, docs, expected):
    jobs = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())["jobs"]
    actual = []
    for name in ("lint-docs", "lint", "build-linux-x64"):
        expression = jobs[name]["if"].removeprefix("${{").removesuffix("}}")
        for key, value in {
            "needs.ci-mode.outputs.mode": mode,
            "needs.ci-mode.outputs.trusted": trusted,
            "needs.path-selection.outputs.docs_only": docs,
            "needs.lint.result": "success",
        }.items():
            expression = expression.replace(key, repr(value))
        expression = expression.replace("always()", "True")
        expression = expression.replace("&&", " and ").replace("||", " or ")
        tree = ast.parse(expression.strip(), mode="eval")
        allowed = (
            ast.Expression,
            ast.BoolOp,
            ast.Compare,
            ast.Constant,
            ast.And,
            ast.Or,
            ast.Eq,
            ast.NotEq,
        )
        assert all(isinstance(node, allowed) for node in ast.walk(tree))
        actual.append(
            eval(compile(tree, "<workflow condition>", "eval"), {"__builtins__": {}})
        )
    assert tuple(actual) == expected
    assert jobs["ci-summary"]["env"]["CI_TRUSTED"] == (
        "${{ needs.ci-mode.outputs.mode == 'minimal' && needs.ci-mode.outputs.trusted == 'true' }}"
    )


@pytest.mark.parametrize("job", ["full-coverage", "ci-summary"])
def test_enforcement_executes_reviewed_base_helpers_and_contracts(job):
    jobs = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())["jobs"]
    checkout = next(
        step for step in jobs[job]["steps"] if "checkout@" in step.get("uses", "")
    )
    assert (
        checkout["with"]["ref"]
        == "${{ github.event.pull_request.base.sha || github.sha }}"
    )
    assert checkout["with"]["persist-credentials"] is False


def test_path_policy_is_trusted_but_runs_diff_in_candidate_checkout():
    jobs = yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())["jobs"]
    checkouts = [
        step["with"]
        for step in jobs["path-selection"]["steps"]
        if "checkout@" in step.get("uses", "")
    ]
    assert any(
        step.get("path") == ".ci-policy"
        and step["ref"] == "${{ github.event.pull_request.base.sha || github.sha }}"
        for step in checkouts
    )
    assert any(
        ".ci-policy/.github/scripts/ci_path_policy.py" in step.get("run", "")
        for step in jobs["path-selection"]["steps"]
    )


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


def test_attested_routine_head_requires_canonical_gate_and_candidate():
    needs = {
        job: {"result": "success"} for job in ["ci-mode", "path-selection", "docs-lint"]
    }
    assert check(needs, mode="minimal", author_permission="write", trusted=True) == []
    assert check(
        needs,
        mode="minimal",
        author_permission="write",
        trusted=True,
        selected_sha="b" * 40,
    )
    needs["ci-mode"] = {"result": "failure"}
    assert check(needs, mode="minimal", author_permission="write", trusted=True)


@pytest.mark.parametrize("mode", ["test", "full"])
def test_attestation_cannot_replace_requested_execution(mode):
    assert "attested skip is limited to a writer's minimal PR" in check(
        passing(), mode=mode, author_permission="write", trusted=True
    )


@pytest.mark.parametrize("event", ["push", "workflow_dispatch", "merge_group"])
def test_attestation_cannot_replace_non_pr_execution(event):
    assert "attested skip is limited to a writer's minimal PR" in check(
        passing(),
        mode="minimal",
        event_name=event,
        author_permission="write",
        trusted=True,
    )


def test_unknown_mode_fails_clearly():
    with pytest.raises(ValueError, match="unknown CI mode"):
        check(passing(), mode="")


def test_merge_queue_requires_full_selected_coverage():
    assert check(passing(), event_name="merge_group") == []
    assert "merge queue requires full CI" in check(
        passing(), event_name="merge_group", mode="minimal"
    )


def test_test_adapter_must_add_real_jobs_beyond_minimal():
    declaration = adapter()
    declaration["test_jobs"] = ["lint", "host"]
    with pytest.raises(ValueError, match="test tier must add"):
        SUMMARY.required_jobs(declaration, full_contract(), "test", False)


@pytest.mark.parametrize("group", ["minimal_jobs", "docs_jobs", "test_jobs"])
def test_policy_checks_cannot_masquerade_as_test_jobs(group):
    declaration = adapter()
    declaration[group] = ["ci-mode"]
    with pytest.raises(ValueError, match="policy prerequisites"):
        SUMMARY.required_jobs(declaration, full_contract(), "full", False)


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


def cli_environment(tmp_path, monkeypatch):
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
    return report_path


def test_cli_failure_report_preserves_candidate_identity(tmp_path, monkeypatch):
    report_path = cli_environment(tmp_path, monkeypatch)
    assert SUMMARY.main() == 1
    report = json.loads(report_path.read_text())
    assert report["schema"] == "fleet-ci-summary/v1"
    assert report["expected_sha"] == SHA
    assert not report["success"]
    assert report["failures"]


@pytest.mark.parametrize(
    "permission, exit_code", [("read", 1), (None, 1), ("admin", 0)]
)
def test_rerun_summary_rechecks_permission_instead_of_cached_write(
    tmp_path, monkeypatch, permission, exit_code
):
    report_path = cli_environment(tmp_path, monkeypatch)
    event_path = tmp_path / "event.json"
    event = {
        "pull_request": {
            "user": {"login": "author"},
            "head": {"sha": SHA},
            "labels": [],
        }
    }
    event_path.write_text(json.dumps(event))
    monkeypatch.setenv("GITHUB_EVENT_PATH", str(event_path))
    monkeypatch.setenv("GITHUB_REPOSITORY", "o/r")
    monkeypatch.setenv("GITHUB_TOKEN", "test-token")
    monkeypatch.setenv("CI_MODE", "minimal")
    monkeypatch.setenv("AUTHOR_PERMISSION", "write")
    monkeypatch.setenv("SELECTED_SHA", SHA)
    queries = []

    def lookup(payload, repository, token):
        queries.append((payload, repository, token))
        return permission

    monkeypatch.setitem(SUMMARY.SELECTOR, "lookup_author_permission", lookup)
    monkeypatch.setitem(
        SUMMARY.SELECTOR, "lookup_current_pull_request", lambda *_: event
    )
    assert SUMMARY.main() == exit_code
    assert queries == [(event, "o/r", "test-token")]
    report = json.loads(report_path.read_text())
    assert report["author_permission"] == (permission or "unknown")


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


@pytest.mark.parametrize("label", ["ci-test", "ci-full"])
def test_rerun_cannot_reuse_minimal_after_current_labels_require_more(label):
    event = {"pull_request": {"head": {"sha": SHA}, "labels": [{"name": label}]}}
    assert SUMMARY.current_selection_failures(
        event, mode="minimal", selected_sha=SHA, permission="admin", aliases={}
    )


def test_current_head_and_legacy_labels_are_authoritative():
    event = {"pull_request": {"head": {"sha": "b" * 40}, "labels": []}}
    assert SUMMARY.current_selection_failures(
        event, mode="full", selected_sha=SHA, permission="admin", aliases={}
    )
    event["pull_request"]["head"]["sha"] = SHA
    event["pull_request"]["labels"] = [{"name": "ci:full"}]
    assert SUMMARY.current_selection_failures(
        event,
        mode="test",
        selected_sha=SHA,
        permission="admin",
        aliases={"ci:full": "ci-full"},
    )


def test_missing_current_metadata_fails_even_when_cached_full_is_green():
    assert SUMMARY.current_selection_failures(
        None, mode="full", selected_sha=SHA, permission="admin", aliases={}
    )


def test_cli_rerun_uses_live_labels_instead_of_original_event(tmp_path, monkeypatch):
    report_path = cli_environment(tmp_path, monkeypatch)
    original = {"pull_request": {"head": {"sha": SHA}, "labels": []}}
    current = {"pull_request": {"head": {"sha": SHA}, "labels": [{"name": "ci-full"}]}}
    event_path = tmp_path / "event.json"
    event_path.write_text(json.dumps(original))
    monkeypatch.setenv("GITHUB_EVENT_PATH", str(event_path))
    monkeypatch.setenv("CI_MODE", "minimal")
    monkeypatch.setenv("SELECTED_SHA", SHA)
    monkeypatch.setitem(
        SUMMARY.SELECTOR, "lookup_current_pull_request", lambda *_: current
    )
    permission_inputs = []

    def permission(payload, *_):
        permission_inputs.append(payload)
        return "admin"

    monkeypatch.setitem(SUMMARY.SELECTOR, "lookup_author_permission", permission)
    assert SUMMARY.main() == 1
    assert permission_inputs == [current]
    assert (
        "current PR requires full CI; cached mode is minimal"
        in json.loads(report_path.read_text())["failures"]
    )
