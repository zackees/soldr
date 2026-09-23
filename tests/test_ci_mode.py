"""The routine CI budget and explicit full-validation contract (soldr#3344)."""

from __future__ import annotations

import json
import re
from pathlib import Path

import pytest
from conftest import load_script_module

ROOT = Path(__file__).parents[1]
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
MODE = load_script_module(ROOT / ".github" / "scripts" / "ci_mode.py", "ci_mode")
COVERAGE = load_script_module(
    ROOT / ".github" / "scripts" / "ci_full_coverage.py", "ci_full_coverage"
)
SHA = "a" * 40


def _job(name: str) -> str:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    match = re.search(rf"^  {re.escape(name)}:\s*$", workflow, re.MULTILINE)
    assert match, name
    end = re.search(r"^  [\w-]+:\s*$", workflow[match.end() :], re.MULTILINE)
    return workflow[match.start() : match.end() + end.start() if end else None]


def test_mode_is_decided_once_and_dispatch_requires_candidate_sha() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert "format('CI full {0}', inputs.candidate_sha)" in workflow
    assert "candidate_sha:" in workflow
    assert (
        "required: true"
        in workflow.split("candidate_sha:", 1)[1].split("permissions:", 1)[0]
    )
    assert "ci_mode.py" in _job("ci-mode")
    assert "mode: ${{ steps.mode.outputs.mode }}" in _job("ci-mode")
    assert "checkout_sha: ${{ steps.mode.outputs.checkout_sha }}" in _job("ci-mode")


def test_docs_only_pr_can_request_full_ci_without_duplicate_lint_status() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    pull_request_trigger = workflow.split("  pull_request:\n", 1)[1].split(
        "  workflow_dispatch:\n", 1
    )[0]
    assert "paths-ignore:" not in pull_request_trigger
    assert not (ROOT / ".github" / "workflows" / "lint-docs-shim.yml").exists()


def test_minimal_jobs_and_full_jobs_have_explicit_dependencies() -> None:
    for name in ("lint", "build-linux-x64"):
        assert "ci-mode" in _job(name)
        assert "needs.ci-mode.outputs.mode == 'full'" not in _job(name)
    full_jobs = (
        "pep517-daemon-smoke",
        "windows-e2e-policy",
        "e2e-cross-bootstrap-soldr",
        "e2e-linux-x64-gnu-build",
        "e2e-linux-arm64-build",
        "e2e-linux-arm64",
        "e2e-linux-x64-musl-build",
        "e2e-linux-x64-musl",
        "e2e-linux-arm64-musl-build",
        "e2e-linux-arm64-musl",
        "e2e-macos-x64-build",
        "e2e-macos-x64",
        "e2e-macos-arm64-build",
        "e2e-macos-arm64",
        "e2e-windows-x64-build",
        "e2e-windows-x64",
        "e2e-windows-x64-gnu-build",
        "e2e-windows-x64-gnu",
        "e2e-windows-arm64-build",
        "e2e-windows-arm64",
        "wheel-cross-policy",
        "wheel-cross-verify",
    )
    for name in full_jobs:
        job = _job(name)
        assert "ci-mode" in job, name
        expected = "!= 'minimal'" if name in {"e2e-cross-bootstrap-soldr", "e2e-macos-arm64-build", "e2e-macos-arm64"} else "== 'full'"
        assert f"needs.ci-mode.outputs.mode {expected}" in job, name
    assert "needs.ci-mode.outputs.mode != 'minimal'" in _job("e2e-linux-x64")
    scheduled = set(
        re.findall(
            r"^  ([\w-]+):\s*$",
            WORKFLOW.read_text().split("jobs:\n", 1)[1],
            re.MULTILINE,
        )
    )
    assert scheduled == set(full_jobs) | {
        "ci-mode",
        "lint",
        "build-linux-x64",
        "e2e-linux-x64",
        "full-coverage",
        "cancel-on-bootstrap-failure",
        "cancel-on-e2e-linux-x64-failure",
    }


def test_manual_run_passes_candidate_sha_to_every_checkout_surface() -> None:
    assert "ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in _job("lint")
    assert "source_ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in _job(
        "build-linux-x64"
    )
    assert "source_ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in _job(
        "e2e-linux-x64"
    )
    assert "ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in _job(
        "e2e-cross-bootstrap-soldr"
    )
    assert "ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in _job(
        "wheel-cross-verify"
    )
    target_run = (ROOT / ".github" / "workflows" / "_ci-target-run.yml").read_text()
    assert "source_ref:" in target_run
    assert (
        "ref: ${{ inputs.source_ref != '' && inputs.source_ref || github.sha }}"
        in target_run
    )
    for name in (
        "e2e-linux-arm64",
        "e2e-linux-x64-musl",
        "e2e-linux-arm64-musl",
        "e2e-windows-x64",
        "e2e-windows-x64-gnu",
        "e2e-windows-arm64",
        "e2e-macos-x64",
        "e2e-macos-arm64",
    ):
        job = _job(name)
        assert "uses: ./.github/workflows/_ci-target-run.yml" in job
        assert "source_ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in job


def test_full_mode_overrides_old_fast_build_policy() -> None:
    assert "--full" in _job("windows-e2e-policy")
    assert "--full" in _job("wheel-cross-policy")
    assert "contains(github.event.pull_request.labels" not in _job(
        "pep517-daemon-smoke"
    )


def test_label_changes_recompute_mode_on_same_head_sha() -> None:
    labels = [{"name": "fast-build"}]
    event = {"pull_request": {"head": {"sha": SHA}, "labels": labels}}
    assert MODE.select_mode("pull_request", event, "") == ("minimal", SHA)
    labels.append({"name": "ci-test"})
    assert MODE.select_mode("pull_request", event, "") == ("test", SHA)
    labels.append({"name": "ci-full"})
    assert MODE.select_mode("pull_request", event, "") == ("full", SHA)
    labels.pop()
    assert MODE.select_mode("pull_request", event, "") == ("test", SHA)
    labels.pop()
    assert MODE.select_mode("pull_request", event, "") == ("minimal", SHA)


def test_ci_test_adds_the_linux_x64_e2e_cell_without_the_full_matrix() -> None:
    assert "needs.ci-mode.outputs.mode != 'minimal'" in _job("e2e-linux-x64")
    assert "needs.ci-mode.outputs.mode == 'full'" in _job("e2e-linux-arm64-build")


def test_dispatch_requires_full_sha_and_push_is_minimal() -> None:
    assert MODE.select_mode("workflow_dispatch", {}, SHA) == ("full", SHA)
    assert MODE.select_mode("push", {"after": SHA}, "") == ("minimal", SHA)
    with pytest.raises(ValueError, match="40-character"):
        MODE.select_mode("workflow_dispatch", {}, "main")
    MODE.verify_checkout(SHA, SHA.upper())
    with pytest.raises(ValueError, match="checked out"):
        MODE.verify_checkout(SHA, "b" * 40)


def test_full_sentinel_rejects_skipped_and_missing_canonical_jobs() -> None:
    contract = {"targets": [{"ci": {"build_job": "build-a", "run_job": "run-a"}}]}
    needs = {job: {"result": "success"} for job in COVERAGE.required_jobs(contract)}
    assert COVERAGE.coverage_failures(contract, needs) == []
    needs["run-a"]["result"] = "skipped"
    assert COVERAGE.coverage_failures(contract, needs) == ["run-a: skipped"]
    del needs["build-a"]
    assert COVERAGE.coverage_failures(contract, needs) == [
        "build-a: missing",
        "run-a: skipped",
    ]


def test_full_sentinel_rejects_build_only_supported_platforms() -> None:
    contract = {
        "targets": [
            {
                "triple": "aarch64-apple-darwin",
                "ci": {
                    "build_job": "build-mac-arm",
                    "run_job": None,
                    "execution_exception": {"issue": 3071},
                },
            }
        ]
    }
    needs = {job: {"result": "success"} for job in COVERAGE.required_jobs(contract)}
    assert COVERAGE.coverage_failures(contract, needs) == [
        "aarch64-apple-darwin: no full-CI execution job (soldr#3071)"
    ]


def test_current_full_contract_requires_both_macos_execution_jobs() -> None:
    contract = json.loads((ROOT / "ci" / "canonical-targets.json").read_text())
    needs = {job: {"result": "success"} for job in COVERAGE.required_jobs(contract)}
    assert COVERAGE.coverage_failures(contract, needs) == []
    needs["e2e-macos-arm64"]["result"] = "skipped"
    assert COVERAGE.coverage_failures(contract, needs) == ["e2e-macos-arm64: skipped"]


def test_native_arm_replay_provisions_rust_objcopy_runtime_before_tests() -> None:
    target_run = (ROOT / ".github" / "workflows" / "_ci-target-run.yml").read_text()
    provision = target_run.index("- name: Provision native ARM rust-objcopy runtime")
    replay = target_run.index("- name: Run owned pre-built native tests")
    assert provision < replay
    step = target_run[provision:replay]
    assert "inputs.target == 'aarch64-apple-darwin'" in step
    assert "python .github/scripts/provision_macos_objcopy.py" in step
    assert '--soldr "$SOLDR_BIN"' in step
    assert '--channel "$RUSTUP_TOOLCHAIN"' in step
    assert '--rustc "$RUSTC"' in step


def test_macos_runner_modes_are_opt_in_and_architecture_matched() -> None:
    contract = json.loads((ROOT / "ci" / "canonical-targets.json").read_text())
    mac_targets = {target["triple"]: target["ci"] for target in contract["targets"] if "apple-darwin" in target["triple"]}
    assert mac_targets["x86_64-apple-darwin"]["runner"] == "macos-15-intel"
    assert mac_targets["aarch64-apple-darwin"]["runner"] == "macos-15"
    for ci in mac_targets.values():
        build = _job(ci["build_job"])
        run = _job(ci["run_job"])
        assert "upload_test_archive: true" in build
        assert "uses: ./.github/workflows/_ci-target-run.yml" in run
        assert f"runs_on: {ci['runner']}" in run
        assert "source_ref: ${{ needs.ci-mode.outputs.checkout_sha }}" in run
        assert "needs.ci-mode.outputs.mode != 'minimal'" in run or "needs.ci-mode.outputs.mode == 'full'" in run
    assert "needs.ci-mode.outputs.mode == 'full'" in _job("e2e-macos-x64")
    assert "needs.ci-mode.outputs.mode != 'minimal'" in _job("e2e-macos-arm64")


def test_full_sentinel_has_all_required_dependencies() -> None:
    contract = json.loads((ROOT / "ci" / "canonical-targets.json").read_text())
    job = _job("full-coverage")
    assert "always()" in job
    assert "needs.ci-mode.outputs.mode == 'full'" in job
    needs_line = next(
        line for line in job.splitlines() if line.startswith("    needs: ")
    )
    dependencies = {
        name.strip() for name in needs_line.split("[", 1)[1].split("]", 1)[0].split(",")
    }
    for required in COVERAGE.required_jobs(contract):
        assert required in dependencies, required


def test_full_overrides_platform_and_wheel_path_policies() -> None:
    windows = load_script_module(
        ROOT / ".github" / "scripts" / "windows_e2e_policy.py", "windows_e2e_full"
    )
    wheel = load_script_module(
        ROOT / ".github" / "scripts" / "wheel_lane_policy.py", "wheel_full"
    )
    assert windows.decide_windows_e2e(
        event_name="pull_request",
        labels=["fast-build"],
        changed_paths=["README.md"],
        full=True,
    ).run
    assert wheel.decide_wheel_lane(
        event_name="pull_request", changed_paths=["README.md"], full=True
    ).run
