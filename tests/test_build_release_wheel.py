from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "build_release_wheel.py"


def _load_script():
    spec = importlib.util.spec_from_file_location("build_release_wheel", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


wheel = _load_script()


@pytest.mark.parametrize(
    "target",
    sorted(wheel.RELEASE_TARGETS),
)
def test_canonical_release_targets_are_accepted(target: str) -> None:
    wheel.validate_target(target)


def test_environment_preserves_unrelated_caller_arguments() -> None:
    env = wheel.build_environment(
        "aarch64-unknown-linux-gnu",
        {"MATURIN_PEP517_ARGS": "--features tokio-console", "KEEP": "yes"},
    )
    assert env["KEEP"] == "yes"
    assert env["MATURIN_PEP517_ARGS"] == "--features tokio-console"
    assert env["SOLDR_PEP517_PROFILE"] == "release"
    assert env["SOLDR_RELEASE_CI"] == "1"


def test_environment_rejects_non_release_pep517_profile() -> None:
    with pytest.raises(ValueError, match="requires SOLDR_PEP517_PROFILE=release"):
        wheel.build_environment(
            "x86_64-unknown-linux-gnu", {"SOLDR_PEP517_PROFILE": "dev"}
        )


def test_unknown_target_is_rejected() -> None:
    with pytest.raises(ValueError, match="unsupported"):
        wheel.validate_target("riscv64gc-unknown-linux-gnu")


def test_hook_forwards_target_as_pep517_config_setting(monkeypatch) -> None:
    observed = {}

    def fake_run(command, *, check, env):
        observed["command"] = command
        observed["check"] = check
        observed["env"] = env

    monkeypatch.setattr(wheel.subprocess, "run", fake_run)
    wheel.run_hook(
        target="x86_64-pc-windows-msvc",
        hook="python -m build --wheel",
        base_env={"PATH": "/managed"},
    )

    assert observed["command"][0] == wheel.sys.executable
    assert observed["command"][-2:] == [
        "--config-setting",
        "target=x86_64-pc-windows-msvc",
    ]
    assert observed["check"] is True
    assert observed["env"]["PATH"] == "/managed"


def test_github_env_loader_preserves_non_shell_identifier(tmp_path: Path) -> None:
    env_file = tmp_path / "github-env"
    env_file.write_text(
        "PKG_CONFIG_PATH_x86_64-unknown-linux-gnu=/managed/pkgconfig\n"
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS=-C link-self-contained=no\n",
        encoding="utf-8",
    )
    assert wheel.read_github_env(env_file) == {
        "PKG_CONFIG_PATH_x86_64-unknown-linux-gnu": "/managed/pkgconfig",
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS": "-C link-self-contained=no",
    }


def test_github_env_loader_rejects_malformed_record(tmp_path: Path) -> None:
    env_file = tmp_path / "github-env"
    env_file.write_text("missing-separator\n", encoding="utf-8")
    with pytest.raises(ValueError, match="invalid GitHub environment record"):
        wheel.read_github_env(env_file)
