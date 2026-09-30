"""Shape of the fast pre-check workflow and its callers (zackees/ci.yml#6).

`ci-pre.yml` (the owner's name for this workflow) holds the cache janitor and
the Actions-cache budget verdict. These tests pin the parts
of the contract that YAML cannot enforce on itself: it installs nothing, it is
fast, it never blocks a build, and every trigger reaches one janitor.
"""

from __future__ import annotations

import ast
import sys
from pathlib import Path

import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"
BUDGET_SCRIPT = REPO_ROOT / ".github" / "scripts" / "check_cache_budget.py"


def _load(name: str) -> dict:
    return yaml.safe_load((WORKFLOWS / name).read_text(encoding="utf-8"))


def _triggers(document: dict) -> dict:
    # PyYAML's YAML 1.1 resolver turns the unquoted key `on` into True.
    triggers = document.get("on", document.get(True))
    return triggers if isinstance(triggers, dict) else {triggers: None}


def test_ci_pre_is_a_reusable_workflow_only() -> None:
    assert set(_triggers(_load("ci-pre.yml"))) == {"workflow_call"}


def test_ci_pre_installs_nothing_and_is_fast() -> None:
    for job_id, job in _load("ci-pre.yml")["jobs"].items():
        steps = " ".join(
            f"{step.get('uses', '')} {step.get('run', '')}" for step in job["steps"]
        )
        for forbidden in ("setup-uv", "setup-python", "uv run", "pip install"):
            assert forbidden not in steps, (job_id, forbidden)
        assert job["timeout-minutes"] <= 2, job_id
        assert job["runs-on"] == "ubuntu-24.04", job_id
        checkouts = [s for s in job["steps"] if "checkout" in s.get("uses", "")]
        assert len(checkouts) == 1, job_id
        assert checkouts[0]["with"]["sparse-checkout"], job_id


def test_the_janitor_is_one_repo_wide_uncancelled_sweep() -> None:
    janitor = _load("ci-pre.yml")["jobs"]["cache-janitor"]
    assert janitor["concurrency"] == {
        "group": "cache-janitor",
        "cancel-in-progress": False,
    }
    assert janitor["permissions"]["actions"] == "write"
    run = " ".join(step.get("run", "") for step in janitor["steps"])
    for flag in ("--prune", "--apply", "--sweep-only", "--require-live"):
        assert flag in run
    # Sweeps on every event with a writable token: skips fork PRs only.
    assert "github.event_name != 'pull_request'" in janitor["if"]
    assert "head.repo.full_name == github.repository" in janitor["if"]


def test_the_verdict_keeps_its_check_name_and_never_waits() -> None:
    verdict = _load("ci-pre.yml")["jobs"]["cache-budget"]
    assert verdict["name"] == "Repository Actions cache budget"
    assert verdict["permissions"]["actions"] == "read"
    # soldr#3342: the post-main-CI sweep event must not run a verdict that races
    # the deletions it triggers, while every other event keeps it.
    assert verdict["if"] == "${{ github.event_name != 'workflow_run' }}"
    run = " ".join(step.get("run", "") for step in verdict["steps"])
    assert "--event-name" in run and "--ref" in run


def test_only_the_janitor_is_serialized() -> None:
    document = _load("ci-pre.yml")
    assert "concurrency" not in document, "no workflow-level group"
    assert document["name"] == "CI Pre"
    jobs = document["jobs"]
    assert jobs["cache-janitor"]["concurrency"] == {
        "group": "cache-janitor",
        "cancel-in-progress": False,
    }
    for job_id, job in jobs.items():
        if job_id == "cache-janitor":
            continue
        assert "concurrency" not in job, job_id
        needs = job.get("needs") or []
        needs = [needs] if isinstance(needs, str) else needs
        assert "cache-janitor" not in needs, job_id


def test_ci_calls_ci_pre_first_and_nothing_waits_for_it() -> None:
    document = _load("ci.yml")
    jobs = document["jobs"]
    assert next(iter(jobs)) == "ci-pre"
    assert jobs["ci-pre"]["uses"] == "./.github/workflows/ci-pre.yml"
    assert jobs["ci-pre"]["permissions"] == {"contents": "read", "actions": "write"}
    for job_id, job in jobs.items():
        needs = job.get("needs") or []
        needs = [needs] if isinstance(needs, str) else needs
        assert "ci-pre" not in needs, job_id
    assert "cache-budget" not in jobs, "the verdict moved into ci-pre.yml"


def test_cache_budget_yml_is_a_thin_caller_with_one_janitor() -> None:
    document = _load("cache-budget.yml")
    triggers = _triggers(document)
    # Every push except main (ci.yml's own push trigger covers main).
    assert triggers["push"] == {"branches-ignore": ["main"]}
    assert "schedule" in triggers and "workflow_dispatch" in triggers
    # soldr#3342: sweep again after main's CI has saved its new cache generation.
    assert triggers["workflow_run"] == {
        "workflows": ["CI"],
        "types": ["completed"],
        "branches": ["main"],
    }
    assert triggers["pull_request"] == {"types": ["closed"]}
    assert "concurrency" not in document, "the janitor job owns concurrency"
    jobs = document["jobs"]
    assert jobs["ci-pre"]["uses"] == "./.github/workflows/ci-pre.yml"
    assert jobs["ci-pre"]["if"] == "${{ github.event_name != 'pull_request' }}"
    text = (WORKFLOWS / "cache-budget.yml").read_text(encoding="utf-8")
    assert "--prune" not in text, "sweeping lives only in ci-pre.yml"


def test_the_closed_pr_job_targets_only_that_prs_ref() -> None:
    job = _load("cache-budget.yml")["jobs"]["closed-pr-cleanup"]
    assert "github.event.action == 'closed'" in job["if"]
    step = next(s for s in job["steps"] if "run" in s)
    assert step["env"]["PR_REF"] == (
        "refs/pull/${{ github.event.pull_request.number }}/merge"
    )
    assert step["run"].strip().endswith('--delete-ref "$PR_REF"')
    assert "--prune" not in step["run"]
    checkout = next(s for s in job["steps"] if "checkout" in s.get("uses", ""))
    assert checkout["with"]["ref"] == "${{ github.event.repository.default_branch }}"


def test_the_budget_script_is_stdlib_only_with_a_runtime_floor() -> None:
    tree = ast.parse(BUDGET_SCRIPT.read_text(encoding="utf-8"))
    imported: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            imported.update(alias.name.split(".")[0] for alias in node.names)
        elif isinstance(node, ast.ImportFrom) and node.module:
            imported.add(node.module.split(".")[0])
    non_stdlib = {
        name
        for name in imported
        if name != "__future__" and name not in sys.stdlib_module_names
    }
    assert not non_stdlib, non_stdlib
    assert "STDLIB_PYTHON_FLOOR = (3, 10)" in BUDGET_SCRIPT.read_text(encoding="utf-8")
