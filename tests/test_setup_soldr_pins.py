from __future__ import annotations

import importlib.util
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
VERIFY_SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "verify_setup_soldr_pin.py"


def load_verify_module():
    spec = importlib.util.spec_from_file_location(
        "verify_setup_soldr_pin", VERIFY_SCRIPT_PATH
    )
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def executable_yaml(text: str) -> str:
    return "\n".join(
        line for line in text.splitlines() if not line.lstrip().startswith("#")
    )


def test_workflows_pin_setup_soldr_to_current_v0_sha() -> None:
    module = load_verify_module()

    module.verify_setup_soldr_pins(REPO_ROOT)


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

    assert "- name: Reset stale cache fallback artifacts" not in executable_yaml(bootstrap)
    assert "- name: Restore checkout after soldr-cook" not in executable_yaml(build)
