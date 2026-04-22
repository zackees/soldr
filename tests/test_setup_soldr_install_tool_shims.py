from __future__ import annotations

import importlib.util
import os
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "install_tool_shims.py"


def _load_module():
    spec = importlib.util.spec_from_file_location("install_tool_shims", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec is not None
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_parse_requested_tools_expands_groups() -> None:
    module = _load_module()

    assert module.parse_requested_tools("false") == []
    assert module.parse_requested_tools("cargo") == ["cargo"]
    assert module.parse_requested_tools("cargo,rustc,cargo") == ["cargo", "rustc"]
    assert module.parse_requested_tools("rust") == [
        "cargo",
        "rustc",
        "rustfmt",
        "clippy-driver",
        "rustdoc",
    ]


def test_tool_env_name_sanitizes_tool_names() -> None:
    module = _load_module()

    assert module.tool_env_name("cargo") == "SOLDR_REAL_CARGO"
    assert module.tool_env_name("clippy-driver") == "SOLDR_REAL_CLIPPY_DRIVER"


def test_main_writes_cargo_shim_and_real_tool_env(
    tmp_path: Path, monkeypatch
) -> None:
    module = _load_module()
    github_env = tmp_path / "github.env"
    github_path = tmp_path / "github.path"
    shim_dir = tmp_path / "shims"
    real_cargo = tmp_path / ("cargo.exe" if os.name == "nt" else "cargo")
    real_cargo.write_text("real cargo", encoding="utf-8")

    monkeypatch.setenv("SETUP_SOLDR_TOOL_SHIMS", "cargo")
    monkeypatch.setenv("SETUP_SOLDR_PATH", str(tmp_path / "soldr"))
    monkeypatch.setenv("SETUP_SOLDR_SHIM_DIR", str(shim_dir))
    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv("GITHUB_PATH", str(github_path))
    monkeypatch.setattr(module, "resolve_tool", lambda tool: str(real_cargo))

    module.main()

    assert f"SOLDR_REAL_CARGO={real_cargo}\n" == github_env.read_text(encoding="utf-8")
    assert f"{shim_dir}\n" == github_path.read_text(encoding="utf-8")

    shim_name = "cargo.cmd" if os.name == "nt" else "cargo"
    shim = shim_dir / shim_name
    text = shim.read_text(encoding="utf-8")
    assert "soldr" in text
    assert "cargo" in text
    if os.name != "nt":
        assert os.access(shim, os.X_OK)
