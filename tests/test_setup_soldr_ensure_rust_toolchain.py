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


def test_rustup_init_url_uses_official_static_download(monkeypatch) -> None:
    module = _load_module()

    monkeypatch.setattr(module.platform, "system", lambda: "Windows")
    monkeypatch.setattr(module.platform, "machine", lambda: "AMD64")

    assert (
        module.rustup_init_url()
        == "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe"
    )


def test_main_bootstraps_rustup_when_missing_and_exports_rustup_toolchain(
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
    bootstrap_installed = {"value": False}

    def fake_which(name: str) -> str | None:
        if name == "rustup":
            return "C:/tools/rustup.exe" if bootstrap_installed["value"] else None
        return {
            "cargo": "C:/tools/cargo.exe",
            "rustc": "C:/tools/rustc.exe",
        }.get(name)

    monkeypatch.setattr(module, "download_rustup_init", lambda _: "C:/tools/rustup-init.exe")
    monkeypatch.setattr(module.shutil, "which", fake_which)

    def fake_run(command):
        commands.append(command)
        if command[0] == "C:/tools/rustup-init.exe":
            bootstrap_installed["value"] = True

    monkeypatch.setattr(module, "run", fake_run)

    module.main()

    assert commands == [
        [
            "C:/tools/rustup-init.exe",
            "-y",
            "--no-modify-path",
            "--default-toolchain",
            "none",
        ],
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
