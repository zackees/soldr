"""Behavioral tests for the native Apple Silicon rust-objcopy preflight."""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest
from conftest import load_script_module

SCRIPT = Path(__file__).parents[1] / ".github" / "scripts" / "provision_macos_objcopy.py"
HELPER = load_script_module(SCRIPT, "provision_macos_objcopy")


def test_installs_llvm_tools_in_pinned_channel_then_probes_objcopy(tmp_path: Path) -> None:
    rustc = tmp_path / "toolchain" / "bin" / "rustc"
    rustc.parent.mkdir(parents=True)
    rustc.touch()
    objcopy = (
        rustc.parent.parent / "lib" / "rustlib" / "aarch64-apple-darwin" / "bin" / "rust-objcopy"
    )
    objcopy.parent.mkdir(parents=True)
    objcopy.touch()
    calls: list[object] = []

    def record(command: object, *, check: object) -> None:
        assert check is True
        calls.append(command)

    assert (
        HELPER.provision_objcopy(Path("soldr"), channel="1.98.1", rustc=rustc, run=record)
        == objcopy
    )
    assert calls == [
        ["soldr", "rustup", "component", "add", "--toolchain", "1.98.1", "llvm-tools-preview"],
        [str(objcopy), "--version"],
    ]


def test_missing_tool_fails_before_replay(tmp_path: Path) -> None:
    rustc = tmp_path / "toolchain" / "bin" / "rustc"
    with pytest.raises(FileNotFoundError, match="no rust-objcopy"):
        HELPER.provision_objcopy(
            Path("soldr"), channel="1.98.1", rustc=rustc, run=lambda *_args, **_kwargs: None
        )


def test_failed_runtime_probe_is_a_hard_error(tmp_path: Path) -> None:
    rustc = tmp_path / "toolchain" / "bin" / "rustc"
    objcopy = (
        rustc.parent.parent / "lib" / "rustlib" / "aarch64-apple-darwin" / "bin" / "rust-objcopy"
    )
    objcopy.parent.mkdir(parents=True)
    objcopy.touch()

    def fail_probe(command: object, *, check: object) -> None:
        assert check is True
        if isinstance(command, list) and command[0] == str(objcopy):
            raise subprocess.CalledProcessError(6, command)

    with pytest.raises(subprocess.CalledProcessError):
        HELPER.provision_objcopy(Path("soldr"), channel="1.98.1", rustc=rustc, run=fail_probe)


@pytest.mark.parametrize("channel,rustc", [("", "/abs/rustc"), ("1.98.1", "relative/rustc")])
def test_rejects_missing_channel_or_nonabsolute_rustc(channel: str, rustc: str) -> None:
    with pytest.raises(ValueError):
        HELPER.provision_objcopy(Path("soldr"), channel=channel, rustc=Path(rustc))


def test_cli_reports_component_failure(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def fail(*_args: Any, **_kwargs: Any) -> None:
        raise subprocess.CalledProcessError(1, ["soldr"])

    monkeypatch.setattr(HELPER, "provision_objcopy", fail)
    assert HELPER.main(["--soldr", "soldr", "--channel", "1.98.1", "--rustc", "/abs/rustc"]) == 1
    assert "runtime preflight failed" in capsys.readouterr().err
