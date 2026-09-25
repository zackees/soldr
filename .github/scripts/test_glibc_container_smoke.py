"""Tests for glibc_container_smoke.py (soldr#1060 item 3).

`verify_glibc_baseline.py` proves a binary's declared symbol floor
statically; this script proves the binary actually starts inside an old
glibc environment. The two failure modes worth pinning: docker missing, and
a container run that "succeeds" (exit 0) but printed nothing, which would
otherwise be a false pass.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from _script_loader import load_script_module

SCRIPT = Path(__file__).resolve().parent / "glibc_container_smoke.py"


@pytest.fixture()
def mod():
    return load_script_module(SCRIPT, "glibc_container_smoke")


class _Result:
    def __init__(self, returncode: int, stdout: str = "", stderr: str = ""):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def test_missing_docker_fails(mod, monkeypatch, tmp_path):
    monkeypatch.setattr(mod.shutil, "which", lambda name: None)
    binary = tmp_path / "soldr"
    binary.write_bytes(b"\x7fELF")
    assert mod.main([str(binary)]) == 1


def test_missing_binary_fails(mod, monkeypatch):
    monkeypatch.setattr(mod.shutil, "which", lambda name: "/usr/bin/docker")
    assert mod.main(["/nonexistent/soldr"]) == 1


def test_successful_run_passes(mod, monkeypatch, tmp_path):
    monkeypatch.setattr(mod.shutil, "which", lambda name: "/usr/bin/docker")
    binary = tmp_path / "soldr"
    binary.write_bytes(b"\x7fELF")

    def fake_run(command, capture_output, text, check):
        assert command[0] == "docker"
        assert "--version" in command
        return _Result(0, stdout="soldr 0.9.21\n")

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    assert mod.main([str(binary)]) == 0


def test_nonzero_exit_fails(mod, monkeypatch, tmp_path):
    monkeypatch.setattr(mod.shutil, "which", lambda name: "/usr/bin/docker")
    binary = tmp_path / "soldr"
    binary.write_bytes(b"\x7fELF")

    def fake_run(command, capture_output, text, check):
        return _Result(1, stdout="", stderr="version `GLIBC_2.39' not found\n")

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    assert mod.main([str(binary)]) == 1


def test_silent_success_is_treated_as_suspicious(mod, monkeypatch, tmp_path):
    # A binary that exits 0 but prints nothing is not the honest "it ran and
    # reported its version" evidence this check exists to gather.
    monkeypatch.setattr(mod.shutil, "which", lambda name: "/usr/bin/docker")
    binary = tmp_path / "soldr"
    binary.write_bytes(b"\x7fELF")

    def fake_run(command, capture_output, text, check):
        return _Result(0, stdout="")

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    assert mod.main([str(binary)]) == 1


def test_default_image_is_manylinux2014(mod):
    # manylinux2014 is CentOS-7-based, glibc 2.17 -- the exact soldr#1060
    # target baseline and the same one `manylinux_2_17` names.
    assert "manylinux2014" in mod.DEFAULT_IMAGE


def test_custom_image_is_used(mod, monkeypatch, tmp_path):
    monkeypatch.setattr(mod.shutil, "which", lambda name: "/usr/bin/docker")
    binary = tmp_path / "soldr"
    binary.write_bytes(b"\x7fELF")
    seen = {}

    def fake_run(command, capture_output, text, check):
        seen["command"] = command
        return _Result(0, stdout="soldr 0.9.21\n")

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    assert mod.main(["--image", "centos:7", str(binary)]) == 0
    assert "centos:7" in seen["command"]
