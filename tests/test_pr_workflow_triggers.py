"""Focused RED/GREEN coverage for the single PR workflow-entry invariant."""

import importlib.util
from pathlib import Path

import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "check_pr_workflow_triggers.py"
SPEC = importlib.util.spec_from_file_location("check_pr_workflow_triggers", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def test_pull_request_in_non_ci_workflow_is_rejected(tmp_path: Path) -> None:
    workflows = tmp_path / "workflows"
    workflows.mkdir()
    (workflows / "ci.yml").write_text("on:\n  pull_request:\n", encoding="utf-8")
    offender = workflows / "other.yml"
    offender.write_text("on:\n  pull_request:\n", encoding="utf-8")

    errors = MODULE.check(workflows)

    assert len(errors) == 1
    assert str(offender) in errors[0]
    assert "pull_request" in errors[0]


def test_pull_request_target_in_non_ci_yaml_workflow_is_rejected(
    tmp_path: Path,
) -> None:
    workflows = tmp_path / "workflows"
    workflows.mkdir()
    (workflows / "ci.yml").write_text("on:\n  pull_request:\n", encoding="utf-8")
    offender = workflows / "other.yaml"
    offender.write_text("on:\n  pull_request_target:\n", encoding="utf-8")

    errors = MODULE.check(workflows)

    assert len(errors) == 1
    assert str(offender) in errors[0]
    assert "pull_request_target" in errors[0]


def test_scalar_and_sequence_pr_triggers_are_rejected(tmp_path: Path) -> None:
    workflows = tmp_path / "workflows"
    workflows.mkdir()
    (workflows / "ci.yml").write_text("on: pull_request\n", encoding="utf-8")
    scalar = workflows / "scalar.yml"
    scalar.write_text("on: pull_request\n", encoding="utf-8")
    sequence = workflows / "sequence.yaml"
    sequence.write_text("on: [push, pull_request_target]\n", encoding="utf-8")

    errors = MODULE.check(workflows)

    assert len(errors) == 2
    assert any(str(scalar) in error and "pull_request" in error for error in errors)
    assert any(
        str(sequence) in error and "pull_request_target" in error for error in errors
    )


def test_comments_strings_and_workflow_call_do_not_violate_the_invariant(
    tmp_path: Path,
) -> None:
    workflows = tmp_path / "workflows"
    workflows.mkdir()
    (workflows / "ci.yml").write_text("on:\n  pull_request:\n", encoding="utf-8")
    (workflows / "reusable.yaml").write_text(
        "# pull_request is discussed here only\n"
        "name: 'pull_request example'\n"
        "on:\n  workflow_call:\n",
        encoding="utf-8",
    )

    assert MODULE.check(workflows) == []


def test_real_workflow_tree_has_ci_as_its_only_pr_entry_point() -> None:
    assert MODULE.check(REPO_ROOT / ".github" / "workflows") == []


def test_canonical_ci_owns_docs_and_retained_pr_signals() -> None:
    document = yaml.safe_load(
        (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    )
    triggers = document.get("on", document[True])
    assert "paths-ignore" not in triggers["pull_request"]

    jobs = document["jobs"]
    assert jobs["lint-docs"]["name"] == "Lint"
    assert "docs_only == 'true'" in jobs["lint-docs"]["if"]
    assert "docs_only != 'true'" in jobs["build-linux-x64"]["if"]
    assert jobs["cache-budget"]["if"] == "${{ github.event_name == 'pull_request' }}"
    assert jobs["setup-soldr-action"]["uses"].endswith("setup-soldr-action.yml")
    assert jobs["cook-size-gate"]["uses"].endswith("cook-size-gate.yml")


def test_reusable_workflow_concurrency_does_not_cancel_canonical_ci() -> None:
    workflow_directory = REPO_ROOT / ".github" / "workflows"

    def concurrency_group(name: str) -> str:
        document = yaml.safe_load(
            (workflow_directory / name).read_text(encoding="utf-8")
        )
        return document["concurrency"]["group"]

    canonical_group = concurrency_group("ci.yml")
    reusable_groups = {
        concurrency_group("setup-soldr-action.yml"),
        concurrency_group("cook-size-gate.yml"),
        concurrency_group("macos-recovery-replay.yml"),
    }

    assert canonical_group not in reusable_groups
    assert len(reusable_groups) == 3
