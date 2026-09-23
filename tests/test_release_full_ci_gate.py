"""Release publication requires a completed exact-candidate full CI run."""

from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".github" / "scripts" / "release_full_ci_gate.py"
spec = importlib.util.spec_from_file_location("release_full_ci_gate", SCRIPT)
assert spec and spec.loader
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)
SHA = "a" * 40
REPO = "zackees/soldr"


def proof() -> tuple[dict, list[dict]]:
    run = {
        "event": "workflow_dispatch",
        "path": ".github/workflows/ci.yml",
        "head_branch": "main",
        "repository": {"full_name": REPO},
        "display_title": f"CI full {SHA}",
        "status": "completed",
        "conclusion": "success",
    }
    jobs = [
        {"name": name, "status": "completed", "conclusion": "success"}
        for name in ("CI mode", "Full coverage")
    ]
    return run, jobs


def test_exact_sha_full_run_succeeds() -> None:
    run, jobs = proof()
    gate.validate(run, jobs, sha=SHA, repository=REPO)


@pytest.mark.parametrize(
    "mutation",
    ["wrong_sha", "pr", "feature_branch", "failed_run", "missing", "skipped", "failed"],
)
def test_nonmatching_or_incomplete_proof_is_rejected(mutation: str) -> None:
    run, jobs = proof()
    if mutation == "wrong_sha":
        run["display_title"] = "CI full " + "b" * 40
    elif mutation == "pr":
        run["event"] = "pull_request"
    elif mutation == "feature_branch":
        run["head_branch"] = "feat/spoofed-full-coverage"
    elif mutation == "failed_run":
        run["conclusion"] = "failure"
    elif mutation == "missing":
        jobs.pop()
    else:
        jobs[-1]["conclusion"] = mutation
    with pytest.raises(gate.GateError):
        gate.validate(run, jobs, sha=SHA, repository=REPO)


def test_workflow_requires_gate_before_build_and_preserves_npm_recovery() -> None:
    workflow = yaml.safe_load(
        (ROOT / ".github" / "workflows" / "release-auto.yml").read_text()
    )
    assert "push" not in workflow.get("on", workflow.get(True, {}))
    jobs = workflow["jobs"]
    assert "full_ci_gate" in jobs["build"]["needs"]
    assert "needs.full_ci_gate.result == 'success'" in jobs["build"]["if"]
    assert "needs.prepare.result == 'skipped'" in jobs["publish-npm"]["if"]
    assert "inputs.npm_release_ref != ''" in jobs["publish-npm"]["if"]
    assert "validate_npm_release_recovery.py" in str(jobs["publish-npm"]["steps"])
