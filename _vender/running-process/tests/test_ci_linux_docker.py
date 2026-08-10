from __future__ import annotations

import subprocess
from pathlib import Path

from ci import linux_docker


def test_ensure_docker_engine_running_starts_desktop_when_engine_is_down(monkeypatch) -> None:
    calls: list[tuple[str, object]] = []
    states = iter([False, False, True])

    monkeypatch.setattr(linux_docker, "docker_executable", lambda: "docker")
    monkeypatch.setattr(
        linux_docker,
        "docker_desktop_executable",
        lambda: Path(r"C:\Program Files\Docker\Docker\Docker Desktop.exe"),
    )
    monkeypatch.setattr(linux_docker, "docker_engine_running", lambda **kwargs: next(states))
    monkeypatch.setattr(
        linux_docker,
        "start_docker_desktop",
        lambda **kwargs: calls.append(("start", kwargs["desktop"])),
    )
    monkeypatch.setattr(linux_docker.time, "monotonic", lambda: 0.0)
    monkeypatch.setattr(
        linux_docker.time,
        "sleep",
        lambda seconds: calls.append(("sleep", seconds)),
    )

    docker = linux_docker.ensure_docker_engine_running(timeout_seconds=10.0)

    assert docker == "docker"
    assert calls == [
        ("start", Path(r"C:\Program Files\Docker\Docker\Docker Desktop.exe")),
        ("sleep", 1.0),
    ]


def test_build_image_command_uses_fixed_dockerfile_and_tag() -> None:
    command = linux_docker.build_image_command(
        docker="docker",
        dockerfile=linux_docker.BUILD_DOCKERFILE,
        tag=linux_docker.BUILD_IMAGE_TAG,
        platform="linux/amd64",
        target="build",
    )

    assert command == [
        "docker",
        "build",
        "-f",
        str(linux_docker.BUILD_DOCKERFILE),
        "-t",
        linux_docker.BUILD_IMAGE_TAG,
        "--target",
        "build",
        "--platform",
        "linux/amd64",
        ".",
    ]


def test_run_container_command_mounts_repo_and_dist_dir() -> None:
    command = linux_docker.run_container_command(
        docker="docker",
        image=linux_docker.PYTEST_IMAGE_TAG,
        shell_command="python -V",
        extra_mounts=["C:\\dist-dev:/dist-dev"],
    )

    assert command == [
        "docker",
        "run",
        "--rm",
        "-w",
        "/work",
        "-v",
        f"{linux_docker.ROOT}:/work",
        "-v",
        "C:\\dist-dev:/dist-dev",
        linux_docker.PYTEST_IMAGE_TAG,
        "sh",
        "-lc",
        "python -V",
    ]


def test_main_build_runs_builder_image_then_container(monkeypatch, tmp_path: Path) -> None:
    seen: list[list[str]] = []

    monkeypatch.setattr(linux_docker, "ensure_docker_engine_running", lambda **kwargs: "docker")
    monkeypatch.setattr(
        linux_docker.subprocess,
        "run",
        lambda cmd, cwd, check=False, capture_output=False, text=False: seen.append(
            [str(part) for part in cmd]
        )
        or subprocess.CompletedProcess(cmd, 0, stdout="28.5.1" if capture_output else None),
    )

    result = linux_docker.main(
        [
            "build",
            "--platform",
            "linux/amd64",
            "--output-dir",
            str(tmp_path),
        ]
    )

    assert result == 0
    assert seen[0] == [
        "docker",
        "build",
        "-f",
        str(linux_docker.BUILD_DOCKERFILE),
        "-t",
        linux_docker.BUILD_IMAGE_TAG,
        "--target",
        "build",
        "--platform",
        "linux/amd64",
        ".",
    ]
    assert seen[1][0:3] == ["docker", "run", "--rm"]
    assert any(f"{tmp_path.resolve()}:/dist-dev" == part for part in seen[1])
    assert any("cp /dist/running_process-*.whl /dist-dev/" in part for part in seen[1])


def test_main_lint_builds_lint_image_then_runs_ci_lint(monkeypatch) -> None:
    seen: list[list[str]] = []

    monkeypatch.setattr(linux_docker, "ensure_docker_engine_running", lambda **kwargs: "docker")
    monkeypatch.setattr(
        linux_docker.subprocess,
        "run",
        lambda cmd, cwd, check=False, capture_output=False, text=False: seen.append(
            [str(part) for part in cmd]
        )
        or subprocess.CompletedProcess(cmd, 0, stdout="28.5.1" if capture_output else None),
    )

    result = linux_docker.main(["lint"])

    assert result == 0
    assert seen[0] == [
        "docker",
        "build",
        "-f",
        str(linux_docker.LINT_DOCKERFILE),
        "-t",
        linux_docker.LINT_IMAGE_TAG,
        ".",
    ]
    assert seen[1][0:3] == ["docker", "run", "--rm"]
    assert any(
        "uv run --script install && uv run --no-editable -m ci.lint" in part
        for part in seen[1]
    )
    assert any("running-process-linux-lint-cargo:/root/.cargo" == part for part in seen[1])


def test_main_pytest_uses_existing_wheel_and_runtime_args(monkeypatch, tmp_path: Path) -> None:
    seen: list[list[str]] = []
    wheel = tmp_path / "running_process-3.0.3-cp311-cp311-musllinux_1_2_x86_64.whl"
    wheel.write_text("placeholder", encoding="utf-8")

    monkeypatch.setattr(linux_docker, "ensure_docker_engine_running", lambda **kwargs: "docker")
    monkeypatch.setattr(
        linux_docker.subprocess,
        "run",
        lambda cmd, cwd, check=False, capture_output=False, text=False: seen.append(
            [str(part) for part in cmd]
        )
        or subprocess.CompletedProcess(cmd, 0, stdout="28.5.1" if capture_output else None),
    )

    result = linux_docker.main(
        [
            "pytest",
            "--output-dir",
            str(tmp_path),
            "--pytest-args",
            "tests/test_pty_support.py -k chain_next_expect -ra",
        ]
    )

    assert result == 0
    assert seen[0] == [
        "docker",
        "build",
        "-f",
        str(linux_docker.PYTEST_DOCKERFILE),
        "-t",
        linux_docker.PYTEST_IMAGE_TAG,
        ".",
    ]
    assert seen[1][0:3] == ["docker", "run", "--rm"]
    assert any("cp /dist-dev/running_process-*.whl /tmp/dist/" in part for part in seen[1])
    assert any(
        "export RUNNING_PROCESS_TEST_TIMEOUT_SECONDS=40" in part
        and
        (
            "python -m running_process.cli -- python -m pytest "
            "tests/test_pty_support.py -k chain_next_expect -ra"
        )
        in part
        for part in seen[1]
    )


def test_main_pytest_rejects_multiple_wheels(monkeypatch, tmp_path: Path) -> None:
    (tmp_path / "running_process-3.0.2-cp311.whl").write_text("old", encoding="utf-8")
    (tmp_path / "running_process-3.0.3-cp311.whl").write_text("new", encoding="utf-8")

    monkeypatch.setattr(linux_docker, "ensure_docker_engine_running", lambda **kwargs: "docker")

    result = linux_docker.main(["pytest", "--output-dir", str(tmp_path)])

    assert result == 1


def test_main_debug_builds_tools_image_then_runs_override_command(monkeypatch) -> None:
    seen: list[list[str]] = []

    monkeypatch.setattr(linux_docker, "ensure_docker_engine_running", lambda **kwargs: "docker")
    monkeypatch.setattr(
        linux_docker.subprocess,
        "run",
        lambda cmd, cwd, check=False, capture_output=False, text=False: seen.append(
            [str(part) for part in cmd]
        )
        or subprocess.CompletedProcess(cmd, 0, stdout="28.5.1" if capture_output else None),
    )

    result = linux_docker.main(
        [
            "debug",
            "--command",
            "bash test timeout_does_not_arm_next_expect",
        ]
    )

    assert result == 0
    assert seen[0] == [
        "docker",
        "build",
        "-f",
        str(linux_docker.BUILD_DOCKERFILE),
        "-t",
        linux_docker.DEBUG_IMAGE_TAG,
        "--target",
        "python-tools",
        ".",
    ]
    assert seen[1][0:3] == ["docker", "run", "--rm"]
    assert any(
        "export IN_RUNNING_PROCESS=running-process-cli" in part
        and "RUNNING_PROCESS_SKIP_LINUX_DOCKER=1" in part
        and "bash test timeout_does_not_arm_next_expect" in part
        for part in seen[1]
    )
    assert any("running-process-linux-debug-cargo:/root/.cargo" == part for part in seen[1])
