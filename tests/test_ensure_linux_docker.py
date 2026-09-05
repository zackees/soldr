"""Unit tests for the Windows Docker engine selection helper."""

import importlib.util
from pathlib import Path

import pytest

SCRIPT = Path(__file__).parents[1] / ".github" / "scripts" / "ensure_linux_docker.py"
WORKFLOW = (
    Path(__file__).parents[1] / ".github" / "workflows" / "docker-linux-cross-smoke.yml"
)
_spec = importlib.util.spec_from_file_location("ensure_linux_docker", SCRIPT)
assert _spec and _spec.loader
module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(module)


def test_docker_info_command_checks_server_os() -> None:
    assert module.docker_info_command() == ["docker", "info", "--format", "{{.OSType}}"]


def test_docker_cli_path_uses_program_files() -> None:
    program_files = Path(r"C:\\Program Files")
    assert module.docker_cli_path(str(program_files)) == (
        program_files / "Docker" / "Docker" / "DockerCli.exe"
    )


def test_docker_cli_path_requires_program_files() -> None:
    with pytest.raises(ValueError, match="ProgramFiles"):
        module.docker_cli_path(None)


def test_docker_cross_smoke_requires_a_linux_engine_hil_runner() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")

    assert "workflow_dispatch:" in workflow
    assert "pull_request:" not in workflow
    assert "schedule:" not in workflow
    assert "runs-on: [self-hosted, Windows, docker-linux]" in workflow
    assert "ensure_linux_docker.py" in workflow
