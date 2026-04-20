from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "ensure_rust_toolchain.py"


def _load_module():
    spec = importlib.util.spec_from_file_location("ensure_rust_toolchain", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec is not None
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_main_installs_requested_toolchain_and_exports_rustup_toolchain(
    tmp_path: Path, monkeypatch
) -> None:
    module = _load_module()
    github_env = tmp_path / "github.env"
    github_output = tmp_path / "github.output"

    monkeypatch.setenv("CARGO_HOME", str(tmp_path / "cargo"))
    monkeypatch.setenv("RUSTUP_HOME", str(tmp_path / "rustup"))
    monkeypatch.setenv("SOLDR_CACHE_DIR", str(tmp_path / "soldr"))
    monkeypatch.setenv("SETUP_SOLDR_TOOLCHAIN_CHANNEL", "1.94.1")
    monkeypatch.setenv("SETUP_SOLDR_TOOLCHAIN_PROFILE", "minimal")
    monkeypatch.setenv("SETUP_SOLDR_TOOLCHAIN_COMPONENTS", '["rustfmt", "clippy"]')
    monkeypatch.setenv("SETUP_SOLDR_TOOLCHAIN_TARGETS", '["x86_64-unknown-linux-musl"]')
    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))

    commands: list[list[str]] = []

    def fake_which(name: str) -> str | None:
        return {
            "rustup": "C:/tools/rustup.exe",
            "cargo": "C:/tools/cargo.exe",
            "rustc": "C:/tools/rustc.exe",
        }.get(name)

    monkeypatch.setattr(module.shutil, "which", fake_which)
    monkeypatch.setattr(module, "run", lambda command: commands.append(command))

    module.main()

    assert commands == [
        ["C:/tools/rustup.exe", "set", "profile", "minimal"],
        [
            "C:/tools/rustup.exe",
            "toolchain",
            "install",
            "1.94.1",
            "--profile",
            "minimal",
            "--component",
            "rustfmt",
            "--component",
            "clippy",
            "--target",
            "x86_64-unknown-linux-musl",
        ],
        ["C:/tools/rustup.exe", "default", "1.94.1"],
        ["C:/tools/cargo.exe", "--version"],
        ["C:/tools/rustc.exe", "--version"],
    ]
    assert github_env.read_text(encoding="utf-8") == "RUSTUP_TOOLCHAIN=1.94.1\n"
    assert github_output.read_text(encoding="utf-8") == "toolchain=1.94.1\n"


def test_main_exits_when_rustup_is_missing(tmp_path: Path, monkeypatch) -> None:
    module = _load_module()

    monkeypatch.setenv("CARGO_HOME", str(tmp_path / "cargo"))
    monkeypatch.setenv("RUSTUP_HOME", str(tmp_path / "rustup"))
    monkeypatch.setenv("SOLDR_CACHE_DIR", str(tmp_path / "soldr"))
    monkeypatch.setenv("SETUP_SOLDR_TOOLCHAIN_CHANNEL", "1.94.1")
    monkeypatch.setenv("SETUP_SOLDR_TOOLCHAIN_PROFILE", "minimal")
    monkeypatch.setenv("SETUP_SOLDR_TOOLCHAIN_COMPONENTS", "[]")
    monkeypatch.setenv("SETUP_SOLDR_TOOLCHAIN_TARGETS", "[]")
    monkeypatch.setattr(module.shutil, "which", lambda _: None)

    with pytest.raises(SystemExit) as excinfo:
        module.main()

    assert "requires rustup to already be available" in str(excinfo.value)
