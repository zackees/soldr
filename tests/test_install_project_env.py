"""soldr#3519: ``./install`` must never write a project env into a mounted checkout.

``uv venv --no-project`` ignores ``UV_PROJECT_ENVIRONMENT`` and always writes
``.venv`` in the working directory. A container from the soldr image (which
sets ``UV_PROJECT_ENVIRONMENT=/venv``) running ``./install`` -- directly, or
via ``./lint`` / ``./test`` -- against the bind-mounted checkout therefore
left a root-owned ``.venv`` there that broke every host ``uv`` invocation.

These tests run the real script against a fake ``uv`` that records its argv,
so they assert exactly which environment path each exec path targets.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
INSTALL = REPO_ROOT / "install"

pytestmark = pytest.mark.skipif(
    os.name == "nt" or shutil.which("bash") is None,
    reason="./install is a bash script; this exercises its POSIX contract",
)


def _fake_uv(bin_dir: Path, log: Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    uv = bin_dir / "uv"
    uv.write_text(
        f"#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n",
        encoding="utf-8",
    )
    uv.chmod(0o755)


def _run_install(
    workdir: Path, tmp_path: Path, extra_env: dict[str, str]
) -> tuple[subprocess.CompletedProcess[str], list[str]]:
    log = tmp_path / "uv.log"
    bin_dir = tmp_path / "bin"
    _fake_uv(bin_dir, log)
    env = {
        key: value
        for key, value in os.environ.items()
        if key not in ("UV_PROJECT_ENVIRONMENT", "VIRTUAL_ENV", "container")
    }
    env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
    env.update(extra_env)
    result = subprocess.run(
        ["bash", str(INSTALL)],
        cwd=workdir,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    calls = log.read_text(encoding="utf-8").splitlines() if log.exists() else []
    return result, calls


def test_container_project_environment_is_where_the_env_is_written(
    tmp_path: Path,
) -> None:
    checkout = tmp_path / "repo"
    checkout.mkdir()
    venv = tmp_path / "container-venv"

    result, calls = _run_install(
        checkout,
        tmp_path,
        {"UV_PROJECT_ENVIRONMENT": str(venv), "container": "docker"},
    )

    assert result.returncode == 0, result.stderr
    assert calls[0].startswith(f"venv {venv} "), calls
    assert calls[1].startswith(f"pip install --python {venv} "), calls
    assert not (checkout / ".venv").exists()


def test_host_default_stays_in_the_checkout(tmp_path: Path) -> None:
    checkout = tmp_path / "repo"
    checkout.mkdir()

    result, calls = _run_install(checkout, tmp_path, {})

    assert result.returncode == 0, result.stderr
    assert calls[0].startswith("venv .venv --python 3.13 "), calls
    assert calls[1].startswith("pip install --python .venv "), calls


def test_container_without_project_environment_refuses_before_uv(
    tmp_path: Path,
) -> None:
    checkout = tmp_path / "repo"
    checkout.mkdir()

    result, calls = _run_install(checkout, tmp_path, {"container": "podman"})

    assert result.returncode != 0
    assert calls == [], "uv must not run, or it writes .venv into the mount"
    assert "UV_PROJECT_ENVIRONMENT" in result.stderr
    assert "soldr#3519" in result.stderr
    assert not (checkout / ".venv").exists()


def test_unusable_checkout_venv_fails_with_the_remedy(tmp_path: Path) -> None:
    checkout = tmp_path / "repo"
    (checkout / ".venv" / "bin").mkdir(parents=True)
    # The shape a container leaves behind: an interpreter link into a
    # directory the host cannot reach.
    (checkout / ".venv" / "bin" / "python").symlink_to(
        tmp_path / "root-only" / "python3.13"
    )

    result, calls = _run_install(checkout, tmp_path, {})

    assert result.returncode != 0
    assert calls == []
    assert "rm -rf .venv" in result.stderr
    assert "soldr#3519" in result.stderr
