import os
from pathlib import Path

import pytest
from conftest import load_script_module

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".github" / "scripts" / "configure_dylint_cargo_shim.py"


def load_module():
    return load_script_module(SCRIPT, "configure_dylint_cargo_shim")


def test_writes_nightly_pinned_posix_cargo_shim(tmp_path: Path) -> None:
    module = load_module()
    toolchain = "nightly-2026-05-26-x86_64-unknown-linux-gnu"

    shim = module.write_cargo_shim(tmp_path, toolchain, windows=False)

    text = shim.read_text(encoding="utf-8")
    assert shim.name == "cargo"
    assert f'export RUSTUP_TOOLCHAIN="{toolchain}"' in text
    assert f'exec soldr rustup run "{toolchain}" cargo "$@"' in text
    if os.name != "nt":
        assert shim.stat().st_mode & 0o111


def test_rejects_shell_metacharacters_in_toolchain(tmp_path: Path) -> None:
    module = load_module()

    with pytest.raises(ValueError):
        module.write_cargo_shim(tmp_path, "nightly;echo unsafe", windows=False)
