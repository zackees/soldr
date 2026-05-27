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


def test_workflows_pin_setup_soldr_to_current_v0_sha() -> None:
    module = load_verify_module()

    module.verify_setup_soldr_pins(REPO_ROOT)


def test_soldr_self_builds_use_pinned_public_setup_soldr() -> None:
    bootstrap = (
        REPO_ROOT / ".github" / "workflows" / "_bootstrap-e2e.yml"
    ).read_text(encoding="utf-8")

    assert "uses: ./soldr" not in bootstrap
    assert "uses: zackees/setup-soldr@" in bootstrap


def test_ci_resets_restore_key_target_artifacts_before_self_builds() -> None:
    bootstrap = (
        REPO_ROOT / ".github" / "workflows" / "_bootstrap-e2e.yml"
    ).read_text(encoding="utf-8")
    build = (REPO_ROOT / ".github" / "workflows" / "_build-and-test.yml").read_text(
        encoding="utf-8"
    )

    for workflow in (bootstrap, build):
        assert "id: setup_soldr" in workflow
        assert "steps.setup_soldr.outputs.target-cache-restore-status != 'exact-hit'" in workflow
        assert "steps.setup_soldr.outputs.build-cache-restore-status != 'exact-hit'" in workflow
        assert 'Join-Path "target" "${{ inputs.target }}"' in workflow
        assert 'Join-Path $env:ZCCACHE_CACHE_DIR "artifacts"' in workflow

    reset_block = bootstrap.split("Reset stale cache fallback artifacts", 1)[1].split(
        "Build soldr-cli", 1
    )[0]
    assert "contains(inputs.target, 'musl')" not in reset_block
