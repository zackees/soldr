"""Fail-closed release execution contract for soldr#3071."""

from __future__ import annotations

import json
from pathlib import Path

from conftest import load_script_module

ROOT = Path(__file__).parents[1]
CONTRACT = ROOT / "ci" / "canonical-targets.json"
WORKFLOW = ROOT / ".github" / "workflows" / "release-auto.yml"
MODULE = load_script_module(
    ROOT / ".github" / "scripts" / "release_completeness.py",
    "release_execution_contract",
)


def _contract(tmp_path: Path, run_job: str | None) -> Path:
    path = tmp_path / "canonical-targets.json"
    path.write_text(
        json.dumps(
            {
                "targets": [
                    {
                        "triple": "aarch64-apple-darwin",
                        "ci": {
                            "build_job": "e2e-macos-arm64-build",
                            "run_job": run_job,
                        },
                        "release": {
                            "status": "included",
                            "execution_gate": {"required": True, "issue": 3071},
                        },
                    }
                ]
            }
        ),
        encoding="utf-8",
    )
    return path


def _workflow(tmp_path: Path, body: str) -> Path:
    path = tmp_path / "ci.yml"
    path.write_text(f"jobs:\n{body}", encoding="utf-8")
    return path


def test_required_execution_without_run_job_fails_closed(tmp_path: Path) -> None:
    failures = MODULE.release_execution_failures(_contract(tmp_path, None))
    assert failures == [
        "aarch64-apple-darwin: release execution required by soldr#3071 "
        "but ci.run_job is missing"
    ]


def test_required_execution_with_run_job_passes(tmp_path: Path) -> None:
    workflow = _workflow(
        tmp_path,
        "  e2e-macos-arm64:\n"
        "    needs: e2e-macos-arm64-build\n"
        "    uses: ./.github/workflows/_ci-target-run.yml\n"
        "    with:\n"
        "      target: aarch64-apple-darwin\n",
    )
    assert MODULE.release_execution_failures(
        _contract(tmp_path, "e2e-macos-arm64"), workflow
    ) == []


def test_nonexistent_or_detached_run_job_does_not_clear_gate(tmp_path: Path) -> None:
    contract = _contract(tmp_path, "e2e-macos-arm64")
    missing = _workflow(tmp_path, "  another-job:\n    runs-on: ubuntu-24.04\n")
    assert "does not exist" in MODULE.release_execution_failures(contract, missing)[0]

    detached = _workflow(
        tmp_path,
        "  e2e-macos-arm64:\n"
        "    uses: ./.github/workflows/_ci-target-run.yml\n"
        "    with:\n"
        "      target: x86_64-apple-darwin\n",
    )
    assert "not a target-matched replay" in MODULE.release_execution_failures(
        contract, detached
    )[0]

    comment_only = _workflow(
        tmp_path,
        "  e2e-macos-arm64:\n"
        "    # e2e-macos-arm64-build is not a dependency here\n"
        "    uses: ./.github/workflows/_ci-target-run.yml\n"
        "    with:\n"
        "      target: aarch64-apple-darwin\n",
    )
    assert "not a target-matched replay" in MODULE.release_execution_failures(
        contract, comment_only
    )[0]


def test_arm64_contract_is_an_explicit_release_blocker() -> None:
    payload = json.loads(CONTRACT.read_text(encoding="utf-8"))
    arm64 = next(
        row for row in payload["targets"] if row["triple"] == "aarch64-apple-darwin"
    )
    assert arm64["release"]["execution_gate"] == {"required": True, "issue": 3071}


def test_checked_in_contract_blocks_release(capsys) -> None:
    assert MODULE.main(["--verify-execution-contract"]) == 1
    stderr = capsys.readouterr().err
    assert "release execution contract is BLOCKED" in stderr
    assert "aarch64-apple-darwin" in stderr


def test_publish_requires_execution_contract_gate() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    gate = workflow.split("  release_execution_contract:\n", 1)[1].split(
        "\n  publish:", 1
    )[0]
    publish = workflow.split("\n  publish:\n", 1)[1].split(
        "\n  verify_github_release:", 1
    )[0]
    assert "--verify-execution-contract" in gate
    assert "release_execution_contract" in publish.split("    if:", 1)[0]
    assert "needs.release_execution_contract.result == 'success'" in publish

    for name, following in (
        ("publish-pypi", "smoke-published-dylint"),
        ("publish-npm", "release-completeness"),
    ):
        job = workflow.split(f"\n  {name}:\n", 1)[1].split(
            f"\n  {following}:", 1
        )[0]
        assert "release_execution_contract" in job.split("    if:", 1)[0]
        assert "needs.release_execution_contract.result == 'success'" in job

    assert "should_publish_pypi == 'true'" in gate
    assert "should_publish_npm == 'true'" in gate
    assert "inputs.npm_release_ref != ''" in gate
