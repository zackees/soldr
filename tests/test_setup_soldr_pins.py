from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
VERIFY_SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "verify_setup_soldr_pin.py"


def load_verify_module():
    return load_script_module(VERIFY_SCRIPT_PATH, "verify_setup_soldr_pin")


def executable_yaml(text: str) -> str:
    return "\n".join(
        line for line in text.splitlines() if not line.lstrip().startswith("#")
    )


def test_workflows_pin_setup_soldr_to_current_v0_sha() -> None:
    """The workflows pin whatever `setup-soldr@v0` currently resolves to.

    soldr#2013. This is the one non-hermetic test in the suite: it resolves
    the tag over the network with `git ls-remote`. Wiring the suite into CI
    without handling that would fail every PR whenever upstream moved `v0` --
    a failure with no relationship to the change under review, which is how a
    gate gets deleted rather than fixed.

    The distinction that matters is *which step* failed:

    * the **lookup** could not run (offline, timeout, rate limit) -> skip,
      because nothing was verified and claiming a pass would be a lie;
    * the lookup ran and the pin **mismatches** -> fail, which is the entire
      point of the test.

    Skipping on any exception would swallow real pin drift, so the guard is
    scoped to the resolution call alone.
    """
    module = load_verify_module()

    try:
        module.resolve_setup_soldr_v0_sha()
    except Exception as exc:  # any lookup failure means "unknown"
        pytest.skip(f"cannot resolve setup-soldr@v0 ({exc.__class__.__name__}): {exc}")

    # Resolution worked, so a failure from here is a genuine pin mismatch.
    module.verify_setup_soldr_pins(REPO_ROOT)


def test_workflow_paths_discovers_yml_and_yaml(tmp_path: Path) -> None:
    module = load_verify_module()
    workflows = tmp_path / ".github" / "workflows"
    workflows.mkdir(parents=True)
    (workflows / "z-last.yaml").write_text("name: yaml\n", encoding="utf-8")
    (workflows / "a-first.yml").write_text("name: yml\n", encoding="utf-8")
    (workflows / "ignored.txt").write_text("name: no\n", encoding="utf-8")

    assert [path.name for path in module.workflow_paths(tmp_path)] == [
        "a-first.yml",
        "z-last.yaml",
    ]
    assert module.workflow_text(tmp_path) == "name: yml\n\nname: yaml\n"


def test_yaml_workflow_is_verified(tmp_path: Path, monkeypatch) -> None:
    module = load_verify_module()
    workflows = tmp_path / ".github" / "workflows"
    workflows.mkdir(parents=True)
    (workflows / "check.yaml").write_text(
        "name: check\nsteps:\n  - uses: zackees/setup-soldr@not-a-sha\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(module, "resolve_setup_soldr_v0_sha", lambda: "a" * 40)

    try:
        module.verify_setup_soldr_pins(tmp_path)
    except SystemExit as exc:
        assert "must be pinned to a full SHA" in str(exc)
    else:
        raise AssertionError("unfixed .yaml workflow unexpectedly passed verification")


def test_yaml_workflow_is_autofixed(tmp_path: Path) -> None:
    module = load_verify_module()
    workflows = tmp_path / ".github" / "workflows"
    workflows.mkdir(parents=True)
    old = "zackees/setup-soldr@not-a-sha"
    (workflows / "check.yaml").write_text(
        f"name: check\nsteps:\n  - uses: {old}\n", encoding="utf-8"
    )
    (workflows / "other.yml").write_text(
        f"name: other\nsteps:\n  - uses: {old}\n", encoding="utf-8"
    )

    module.update_workflow_pins(tmp_path, "b" * 40)

    for path in (workflows / "check.yaml", workflows / "other.yml"):
        assert "zackees/setup-soldr@" + "b" * 40 in path.read_text(encoding="utf-8")
        assert old not in path.read_text(encoding="utf-8")


def test_soldr_self_builds_use_pinned_public_setup_soldr() -> None:
    bootstrap = (REPO_ROOT / ".github" / "workflows" / "_bootstrap-e2e.yml").read_text(
        encoding="utf-8"
    )

    assert "uses: ./soldr" not in bootstrap
    assert "uses: zackees/setup-soldr@" in bootstrap


def test_ci_does_not_carry_stale_setup_soldr_fallback_resets() -> None:
    bootstrap = (REPO_ROOT / ".github" / "workflows" / "_bootstrap-e2e.yml").read_text(
        encoding="utf-8"
    )
    build = (REPO_ROOT / ".github" / "workflows" / "_build-and-test.yml").read_text(
        encoding="utf-8"
    )

    for workflow in (executable_yaml(bootstrap), executable_yaml(build)):
        assert "id: setup_soldr" not in workflow
        assert "steps.setup_soldr.outputs" not in workflow
        assert "SOLDR_TARGET_CACHE_MODE=off" not in workflow
        assert 'Join-Path "target" "${{ inputs.target }}"' not in workflow
        assert 'Join-Path $env:ZCCACHE_CACHE_DIR "artifacts"' not in workflow

    assert "- name: Reset stale cache fallback artifacts" not in executable_yaml(
        bootstrap
    )
    assert "- name: Restore checkout after soldr-cook" not in executable_yaml(build)
