"""Unit tests for the Windows Docker engine selection helper."""

import importlib.util
from pathlib import Path

import pytest


SCRIPT = Path(__file__).parents[1] / ".github" / "scripts" / "ensure_linux_docker.py"
_spec = importlib.util.spec_from_file_location("ensure_linux_docker", SCRIPT)
assert _spec and _spec.loader
module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(module)


def test_docker_info_command_checks_server_os() -> None:
    assert module.docker_info_command() == ["docker", "info", "--format", "{{.OSType}}"]


def test_docker_cli_path_uses_program_files() -> None:
    assert module.docker_cli_path(r"C:\\Program Files") == Path(
        r"C:\\Program Files\\Docker\\Docker\\DockerCli.exe"
    )


def test_docker_cli_path_requires_program_files() -> None:
    with pytest.raises(ValueError, match="ProgramFiles"):
        module.docker_cli_path(None)
