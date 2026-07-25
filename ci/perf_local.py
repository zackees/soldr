#!/usr/bin/env python3
"""Run Cargo commands in one recycled Docker runner with warm volumes.

The stable ``soldr-perf-local`` container mounts the repository's shared git
root at ``/repo``. Linked worktrees below that root reuse the same runner by
changing only the ``docker exec`` working directory. Cargo target state,
Cargo home, and soldr home live in named volumes and survive runner resets.

Usage::

    uv run --no-project python ci/perf_local.py cargo build --release
    uv run --no-project python ci/perf_local.py cargo test --workspace
    uv run --no-project python ci/perf_local.py --status
    uv run --no-project python ci/perf_local.py --stop
    uv run --no-project python ci/perf_local.py --reset-runner
    uv run --no-project python ci/perf_local.py --wipe

``--stop`` and ``--reset-runner`` preserve warm volumes. ``--wipe`` is the
explicit destructive operation that removes both the runner and its volumes.
"""

from __future__ import annotations

import json
import hashlib
import os
import shutil
import subprocess
import sys
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator

IMAGE = "soldr-cook-dev"
DOCKERFILE = "docker/cook-shared-cache/Dockerfile"
DOCKER_CONTEXT = "docker/cook-shared-cache"
CONTAINER = "soldr-perf-local"
VOLUME_TARGET = "soldr-perf-target"
VOLUME_CARGO_HOME = "soldr-perf-cargo-home"
VOLUME_SOLDR_HOME = "soldr-perf-soldr-home"
RUNNER_SCHEMA = "1"
LABEL_PREFIX = "io.soldr.perf-local"
PTRACE_ENV = "SOLDR_PERF_LOCAL_PTRACE"

USAGE = """\
usage: python ci/perf_local.py cargo <args...>
       python ci/perf_local.py --status
       python ci/perf_local.py --stop
       python ci/perf_local.py --reset-runner
       python ci/perf_local.py --wipe

Every argument after `cargo` is forwarded verbatim to Cargo in the recycled
runner. --stop and --reset-runner preserve named volumes; --wipe removes them.
"""


def main(argv: list[str]) -> int:
    repo_root = Path(__file__).resolve().parent.parent
    source_root = shared_source_root(repo_root)
    os.chdir(repo_root)

    if not shutil.which("docker"):
        print("error: docker not on PATH", file=sys.stderr)
        return 2
    if not argv:
        print(USAGE, file=sys.stderr)
        return 2
    if argv[0] in ("-h", "--help"):
        print(USAGE)
        return 0
    if argv[0] == "--status":
        return status()
    if argv[0] in ("--stop", "--reset-runner", "--wipe"):
        with runner_lock(source_root):
            if argv[0] == "--stop":
                return stop_runner()
            if argv[0] == "--reset-runner":
                return reset_runner()
            return wipe()
    if argv[0] != "cargo":
        print(
            f"error: expected `cargo` as the first arg, got {argv[0]!r}\n\n{USAGE}",
            file=sys.stderr,
        )
        return 2

    # Serialize the full command. Cargo can lock a shared target itself, but
    # parallel LTO builds make every sample slower and invalidate perf data.
    with runner_lock(source_root):
        try:
            image_id = ensure_image(repo_root)
        except RuntimeError as error:
            print(f"error: {error}", file=sys.stderr)
            return 1
        ensure_runner(source_root, image_id)
        workdir = container_workdir(source_root, repo_root)
        completed = subprocess.run(exec_command(argv, workdir, tty=tty_enabled()), check=False)
        return completed.returncode


def shared_source_root(repo_root: Path) -> Path:
    """Return the checkout root whose .git directory owns all worktrees."""
    out = subprocess.run(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        check=False,
    )
    if out.returncode == 0:
        common = Path(out.stdout.strip()).resolve()
        if common.name == ".git":
            return common.parent
    return repo_root.resolve()


def container_workdir(source_root: Path, repo_root: Path) -> str:
    relative = repo_root.resolve().relative_to(source_root.resolve())
    return "/repo" if relative == Path(".") else f"/repo/{relative.as_posix()}"


def tty_enabled() -> bool:
    value = os.environ.get("SOLDR_PERF_LOCAL_TTY", "").strip().lower()
    return value in ("1", "true", "yes")


def expected_labels(source_root: Path, image_id: str) -> dict[str, str]:
    return {
        f"{LABEL_PREFIX}.schema": RUNNER_SCHEMA,
        f"{LABEL_PREFIX}.image-id": image_id,
        f"{LABEL_PREFIX}.source-root": str(source_root.resolve()),
        f"{LABEL_PREFIX}.ptrace": "1" if ptrace_enabled() else "0",
    }


def dockerfile_digest(repo_root: Path) -> str:
    return hashlib.sha256((repo_root / DOCKERFILE).read_bytes()).hexdigest()


def image_info() -> dict[str, object] | None:
    inspected = subprocess.run(
        ["docker", "image", "inspect", IMAGE],
        capture_output=True,
        text=True,
        check=False,
    )
    if inspected.returncode != 0:
        return None
    decoded = json.loads(inspected.stdout)
    return decoded[0] if decoded else None


def ensure_image(repo_root: Path) -> str:
    digest = dockerfile_digest(repo_root)
    digest_label = f"{LABEL_PREFIX}.dockerfile-sha256"
    info = image_info()
    if info is not None:
        config = info.get("Config")
        labels = config.get("Labels") if isinstance(config, dict) else None
        image_id = info.get("Id")
        if isinstance(labels, dict) and labels.get(digest_label) == digest and image_id:
            return str(image_id)

    build = subprocess.run(
        [
            "docker",
            "build",
            "--provenance=false",
            "--label",
            f"{digest_label}={digest}",
            "-f",
            DOCKERFILE,
            "-t",
            IMAGE,
            DOCKER_CONTEXT,
        ],
        check=False,
    )
    if build.returncode != 0:
        raise RuntimeError(f"docker build failed with exit code {build.returncode}")
    info = image_info()
    image_id = info.get("Id") if info is not None else None
    if not image_id:
        raise RuntimeError(f"unable to inspect image {IMAGE} after build")
    return str(image_id)


def runner_matches(info: dict[str, object], labels: dict[str, str]) -> bool:
    config = info.get("Config")
    if not isinstance(config, dict):
        return False
    actual = config.get("Labels")
    return isinstance(actual, dict) and all(
        actual.get(key) == value for key, value in labels.items()
    )


def create_command(source_root: Path, image_id: str) -> list[str]:
    command = ["docker", "create", "--name", CONTAINER, "--init"]
    if ptrace_enabled():
        command.extend(["--cap-add=SYS_PTRACE", "--security-opt", "seccomp=unconfined"])
    for key, value in expected_labels(source_root, image_id).items():
        command.extend(["--label", f"{key}={value}"])
    command.extend(
        [
            "-v",
            f"{source_root.resolve()}:/repo",
            "-v",
            f"{VOLUME_SOLDR_HOME}:/root/.soldr",
            "-v",
            f"{VOLUME_TARGET}:/target",
            "-v",
            f"{VOLUME_CARGO_HOME}:/root/.cargo",
            "-e",
            "CARGO_TARGET_DIR=/target",
            "-w",
            "/repo",
            IMAGE,
            "tail",
            "-f",
            "/dev/null",
        ]
    )
    return command


def ptrace_enabled() -> bool:
    return os.environ.get(PTRACE_ENV, "").strip().lower() in ("1", "true", "yes")


def exec_command(argv: list[str], workdir: str, *, tty: bool) -> list[str]:
    command = ["docker", "exec"]
    if tty:
        command.append("-it")
    command.extend(["-w", workdir, CONTAINER, *argv])
    return command


def ensure_runner(source_root: Path, image_id: str) -> None:
    labels = expected_labels(source_root, image_id)
    inspected = subprocess.run(
        ["docker", "inspect", CONTAINER],
        capture_output=True,
        text=True,
        check=False,
    )
    exists = inspected.returncode == 0
    if exists:
        info = json.loads(inspected.stdout)[0]
        if not runner_matches(info, labels):
            subprocess.run(["docker", "rm", "-f", CONTAINER], check=True)
            exists = False
    if not exists:
        for volume in (VOLUME_TARGET, VOLUME_CARGO_HOME, VOLUME_SOLDR_HOME):
            subprocess.run(["docker", "volume", "create", volume], check=True)
        subprocess.run(create_command(source_root, image_id), check=True)

    running = docker_output(["inspect", "--format", "{{.State.Running}}", CONTAINER])
    if running != "true":
        subprocess.run(["docker", "start", CONTAINER], check=True)


def docker_output(args: list[str]) -> str:
    out = subprocess.run(["docker", *args], capture_output=True, text=True, check=False)
    return out.stdout.strip() if out.returncode == 0 else ""


@contextmanager
def runner_lock(source_root: Path) -> Iterator[None]:
    lock_path = source_root / ".git" / "soldr-perf-local.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+b") as handle:
        if os.name == "nt":
            import msvcrt

            handle.seek(0, os.SEEK_END)
            if handle.tell() == 0:
                handle.write(b"0")
                handle.flush()
            handle.seek(0)
            msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
            try:
                yield
            finally:
                handle.seek(0)
                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
        else:
            import fcntl

            fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
            try:
                yield
            finally:
                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def reset_runner() -> int:
    if not docker_output(["inspect", "--format", "{{.Id}}", CONTAINER]):
        return 0
    return subprocess.run(["docker", "rm", "-f", CONTAINER], check=False).returncode


def stop_runner() -> int:
    if not docker_output(["inspect", "--format", "{{.Id}}", CONTAINER]):
        return 0
    return subprocess.run(["docker", "stop", CONTAINER], check=False).returncode


def wipe() -> int:
    reset_runner()
    out = subprocess.run(
        [
            "docker",
            "volume",
            "rm",
            "--force",
            VOLUME_TARGET,
            VOLUME_CARGO_HOME,
            VOLUME_SOLDR_HOME,
        ],
        check=False,
    )
    return out.returncode


def status() -> int:
    state = docker_output(["inspect", "--format", "{{.State.Status}}", CONTAINER]) or "(absent)"
    print(f"{CONTAINER}  {state}")
    rows: list[tuple[str, str]] = []
    for name in (VOLUME_TARGET, VOLUME_CARGO_HOME, VOLUME_SOLDR_HOME):
        mountpoint = docker_output(["volume", "inspect", "--format", "{{.Mountpoint}}", name])
        rows.append((name, mountpoint or "(absent)"))
    width = max(len(name) for name, _ in rows)
    for name, info in rows:
        print(f"{name:<{width}}  {info}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
