from __future__ import annotations

import argparse
import os
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

from ci.env import repo_root

ROOT = repo_root()
BUILD_DOCKERFILE = ROOT / "Dockerfile.linux-build"
LINT_DOCKERFILE = ROOT / "Dockerfile.linux-lint"
PYTEST_DOCKERFILE = ROOT / "Dockerfile.linux-pytest"
DIST_DEV = ROOT / "dist-dev"
BUILD_IMAGE_TAG = "running-process/linux-build:local"
DEBUG_IMAGE_TAG = "running-process/linux-debug:local"
LINT_IMAGE_TAG = "running-process/linux-lint:local"
PYTEST_IMAGE_TAG = "running-process/linux-pytest:local"
DEFAULT_ENGINE_TIMEOUT_SECONDS = 120.0
DEFAULT_PYTEST_ARGS = ["-m", "not live", "-ra"]
SERVER_VERSION_FORMAT = "{{.Server.Version}}"
WINDOWS_DOCKER_DESKTOP = Path(r"C:\Program Files\Docker\Docker\Docker Desktop.exe")


def docker_executable() -> str:
    docker = shutil.which("docker")
    if docker:
        return docker
    fallback = Path(r"C:\Program Files\Docker\Docker\resources\bin\docker.exe")
    if fallback.is_file():
        return str(fallback)
    raise RuntimeError("docker executable not found on PATH")


def docker_desktop_executable() -> Path | None:
    if os.name != "nt":
        return None
    if WINDOWS_DOCKER_DESKTOP.is_file():
        return WINDOWS_DOCKER_DESKTOP
    return None


def docker_engine_running(*, docker: str | None = None) -> bool:
    docker_cmd = docker or docker_executable()
    result = subprocess.run(
        [docker_cmd, "version", "--format", SERVER_VERSION_FORMAT],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    return result.returncode == 0 and bool(result.stdout.strip())


def start_docker_desktop(*, desktop: Path | None = None) -> None:
    executable = desktop or docker_desktop_executable()
    if executable is None:
        raise RuntimeError("Docker Desktop executable not found")
    subprocess.Popen(
        [str(executable)],
        cwd=executable.parent,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def ensure_docker_engine_running(*, timeout_seconds: float = DEFAULT_ENGINE_TIMEOUT_SECONDS) -> str:
    docker = docker_executable()
    if docker_engine_running(docker=docker):
        return docker
    desktop = docker_desktop_executable()
    if desktop is None:
        raise RuntimeError(
            "docker server is not reachable and no Docker Desktop launcher was found; "
            "start the Docker engine and retry"
        )
    start_docker_desktop(desktop=desktop)
    deadline = time.monotonic() + max(0.0, timeout_seconds)
    while time.monotonic() < deadline:
        if docker_engine_running(docker=docker):
            return docker
        time.sleep(1.0)
    raise RuntimeError(
        f"timed out after {timeout_seconds:.0f}s waiting for the Docker engine to start"
    )


def split_pytest_args(pytest_args: str | None) -> list[str]:
    if not pytest_args:
        return list(DEFAULT_PYTEST_ARGS)
    return shlex.split(pytest_args, posix=True)


def shell_join(parts: list[str]) -> str:
    return " ".join(shlex.quote(part) for part in parts)


def cache_volume(name: str, container_path: str) -> str:
    return f"running-process-{name}:{container_path}"


def build_image_command(
    *,
    docker: str,
    dockerfile: Path,
    tag: str,
    platform: str | None,
    target: str | None = None,
) -> list[str]:
    cmd = [docker, "build", "-f", str(dockerfile), "-t", tag]
    if target:
        cmd.extend(["--target", target])
    if platform:
        cmd.extend(["--platform", platform])
    cmd.append(".")
    return cmd


def run_container_command(
    *,
    docker: str,
    image: str,
    shell_command: str,
    extra_mounts: list[str],
) -> list[str]:
    cmd = [docker, "run", "--rm", "-w", "/work", "-v", f"{ROOT}:/work"]
    for mount in extra_mounts:
        cmd.extend(["-v", mount])
    cmd.extend([image, "sh", "-lc", shell_command])
    return cmd


def output_dir(path: Path | None = None) -> Path:
    return (path or DIST_DEV).resolve()


def wheel_glob(output_path: str = "/dist-dev") -> str:
    return f"{output_path}/running_process-*.whl"


def pytest_mounts(dist_dir: Path) -> list[str]:
    return [
        f"{dist_dir}:/dist-dev",
        cache_volume("alpine-pytest-pip", "/root/.cache/pip"),
    ]


def lint_mounts() -> list[str]:
    return [
        cache_volume("linux-lint-cargo", "/root/.cargo"),
        cache_volume("linux-lint-rustup", "/root/.rustup"),
        cache_volume("linux-lint-uv", "/root/.cache/uv"),
    ]


def debug_mounts() -> list[str]:
    return [
        cache_volume("linux-debug-cargo", "/root/.cargo"),
        cache_volume("linux-debug-rustup", "/root/.rustup"),
        cache_volume("linux-debug-pip", "/root/.cache/pip"),
        cache_volume("linux-debug-uv", "/root/.cache/uv"),
    ]


def build_export_mounts(dist_dir: Path) -> list[str]:
    return [f"{dist_dir}:/dist-dev"]


def build_export_shell_command() -> str:
    return (
        "mkdir -p /dist-dev && "
        "rm -f /dist-dev/running_process-*.whl && "
        "cp /dist/running_process-*.whl /dist-dev/"
    )


def pytest_shell_command(pytest_args: list[str]) -> str:
    return (
        "export RUNNING_PROCESS_TEST_TIMEOUT_SECONDS=40 && "
        + "mkdir -p /tmp/dist && "
        + f"cp {wheel_glob()} /tmp/dist/ && "
        + "python -m pip install --force-reinstall /tmp/dist/running_process-*.whl"
        + " && "
        + shell_join(
            [
                "python",
                "-m",
                "running_process.cli",
                "--",
                "python",
                "-m",
                "pytest",
                *pytest_args,
            ]
        )
    )


def lint_shell_command() -> str:
    # Keep the container's venv out of the mounted /work tree: a host
    # (Windows/macOS) .venv is not usable in the container and uv would
    # otherwise try to delete and recreate it in place.
    return (
        "export UV_PROJECT_ENVIRONMENT=/tmp/running-process-linux-venv && "
        "uv run --script install && uv run --no-editable -m ci.lint"
    )


def debug_shell_command(*, command: str | None, pytest_args: list[str]) -> str:
    prefix = (
        "export IN_RUNNING_PROCESS=running-process-cli "
        "RUNNING_PROCESS_SKIP_LINUX_DOCKER=1 "
        "RUNNING_PROCESS_TEST_COMMAND_TIMEOUT_SECONDS=180 "
        "UV_PROJECT_ENVIRONMENT=/tmp/running-process-linux-venv"
    )
    setup = shell_join(
        [
            "python",
            "-m",
            "pip",
            "install",
            "uv>=0.8,<1",
            "pytest-timeout>=2,<3",
        ]
    )
    run_command = shell_join(
        shlex.split(command, posix=True) if command is not None else ["bash", "test"]
    )
    return " && ".join([prefix, setup, run_command])


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run Alpine Linux wheel build and pytest workflows"
    )
    subparsers = parser.add_subparsers(dest="workflow", required=True)
    for name in ("build", "lint", "pytest", "all", "debug"):
        subparser = subparsers.add_parser(name)
        subparser.add_argument(
            "--platform",
            default=None,
            help="Optional Docker platform such as linux/amd64 or linux/arm64",
        )
        subparser.add_argument(
            "--output-dir",
            type=Path,
            default=None,
            help="Directory to write wheel artifacts to; defaults to dist-dev/",
        )
        subparser.add_argument(
            "--engine-timeout",
            type=float,
            default=DEFAULT_ENGINE_TIMEOUT_SECONDS,
            help="Seconds to wait for Docker Desktop to bring up the engine",
        )
        subparser.add_argument(
            "--no-auto-start",
            action="store_true",
            help="Do not attempt to start Docker Desktop when the engine is unavailable",
        )
    subparsers.choices["pytest"].add_argument(
        "--pytest-args",
        default=None,
        help="Optional pytest arguments passed at container runtime",
    )
    subparsers.choices["all"].add_argument(
        "--pytest-args",
        default=None,
        help="Optional pytest arguments passed at container runtime",
    )
    subparsers.choices["debug"].add_argument(
        "--command",
        default=None,
        help="Optional shell command override to run inside the Linux builder container",
    )
    return parser.parse_args(argv)


def run_logged(cmd: list[str]) -> int:
    return int(subprocess.run(cmd, cwd=ROOT, check=False).returncode)


def build_image(*, docker: str, dockerfile: Path, tag: str, platform: str | None) -> int:
    return run_logged(
        build_image_command(
            docker=docker,
            dockerfile=dockerfile,
            tag=tag,
            platform=platform,
        )
    )


def ensure_single_wheel_exists(dist_dir: Path) -> Path:
    wheels = sorted(dist_dir.glob("running_process-*.whl"))
    if not wheels:
        raise RuntimeError(
            f"no wheel found in {dist_dir}; run `python -m ci.linux_docker build` first"
        )
    if len(wheels) > 1:
        names = ", ".join(wheel.name for wheel in wheels)
        raise RuntimeError(
            f"expected exactly one wheel in {dist_dir}, found {len(wheels)}: {names}"
        )
    return wheels[0]


def run_build_workflow(*, docker: str, platform: str | None, dist_dir: Path) -> int:
    dist_dir.mkdir(parents=True, exist_ok=True)
    if (
        run_logged(
            build_image_command(
                docker=docker,
                dockerfile=BUILD_DOCKERFILE,
                tag=BUILD_IMAGE_TAG,
                platform=platform,
                target="build",
            )
        )
        != 0
    ):
        return 1
    return run_logged(
        run_container_command(
            docker=docker,
            image=BUILD_IMAGE_TAG,
            shell_command=build_export_shell_command(),
            extra_mounts=build_export_mounts(dist_dir),
        )
    )


def run_lint_workflow(*, docker: str, platform: str | None) -> int:
    if (
        build_image(
            docker=docker,
            dockerfile=LINT_DOCKERFILE,
            tag=LINT_IMAGE_TAG,
            platform=platform,
        )
        != 0
    ):
        return 1
    return run_logged(
        run_container_command(
            docker=docker,
            image=LINT_IMAGE_TAG,
            shell_command=lint_shell_command(),
            extra_mounts=lint_mounts(),
        )
    )


def run_pytest_workflow(
    *,
    docker: str,
    platform: str | None,
    dist_dir: Path,
    pytest_args: list[str],
) -> int:
    ensure_single_wheel_exists(dist_dir)
    if (
        build_image(
            docker=docker,
            dockerfile=PYTEST_DOCKERFILE,
            tag=PYTEST_IMAGE_TAG,
            platform=platform,
        )
        != 0
    ):
        return 1
    return run_logged(
        run_container_command(
            docker=docker,
            image=PYTEST_IMAGE_TAG,
            shell_command=pytest_shell_command(pytest_args),
            extra_mounts=pytest_mounts(dist_dir),
        )
    )


def run_debug_workflow(
    *,
    docker: str,
    platform: str | None,
    command: str | None,
    pytest_args: list[str],
) -> int:
    if (
        run_logged(
            build_image_command(
                docker=docker,
                dockerfile=BUILD_DOCKERFILE,
                tag=DEBUG_IMAGE_TAG,
                platform=platform,
                target="python-tools",
            )
        )
        != 0
    ):
        return 1
    return run_logged(
        run_container_command(
            docker=docker,
            image=DEBUG_IMAGE_TAG,
            shell_command=debug_shell_command(command=command, pytest_args=pytest_args),
            extra_mounts=debug_mounts(),
        )
    )


def resolve_docker(args: argparse.Namespace) -> str:
    if args.no_auto_start:
        docker = docker_executable()
        if not docker_engine_running(docker=docker):
            raise RuntimeError("docker server is not reachable; start Docker Desktop and retry")
        return docker
    return ensure_docker_engine_running(timeout_seconds=args.engine_timeout)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    dist_dir = output_dir(args.output_dir)
    try:
        docker = resolve_docker(args)
    except RuntimeError as exc:
        print(str(exc), file=sys.stderr, flush=True)
        return 1

    if args.workflow == "build":
        return run_build_workflow(docker=docker, platform=args.platform, dist_dir=dist_dir)

    if args.workflow == "lint":
        return run_lint_workflow(docker=docker, platform=args.platform)

    if args.workflow == "pytest":
        try:
            return run_pytest_workflow(
                docker=docker,
                platform=args.platform,
                dist_dir=dist_dir,
                pytest_args=split_pytest_args(args.pytest_args),
            )
        except RuntimeError as exc:
            print(str(exc), file=sys.stderr, flush=True)
            return 1

    if args.workflow == "all":
        build_result = run_build_workflow(docker=docker, platform=args.platform, dist_dir=dist_dir)
        if build_result != 0:
            return build_result
        lint_result = run_lint_workflow(docker=docker, platform=args.platform)
        if lint_result != 0:
            return lint_result
        try:
            return run_pytest_workflow(
                docker=docker,
                platform=args.platform,
                dist_dir=dist_dir,
                pytest_args=split_pytest_args(args.pytest_args),
            )
        except RuntimeError as exc:
            print(str(exc), file=sys.stderr, flush=True)
            return 1

    if args.workflow == "debug":
        return run_debug_workflow(
            docker=docker,
            platform=args.platform,
            command=args.command,
            pytest_args=list(DEFAULT_PYTEST_ARGS),
        )

    raise RuntimeError(f"unsupported workflow {args.workflow}")


if __name__ == "__main__":
    raise SystemExit(main())
