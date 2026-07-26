#!/usr/bin/env python3
"""Run Cargo commands in one recycled Docker runner with warm volumes.

A ``soldr-perf-local-<slug>-<hash>`` container mounts the repository's shared
git root at ``/repo``. Linked worktrees below that root reuse the same runner
by changing only the ``docker exec`` working directory. Cargo target state,
Cargo home, and soldr home live in named volumes and survive runner resets.

The container and volume names are derived from the shared git root, so
sibling checkouts (``soldr``, ``soldr2``, ``soldr3``) each get their own
runner and their own warm volumes. They previously shared one global
``soldr-perf-local`` container while locking per-root, so a run started in
one checkout would ``docker rm -f`` another checkout's running container and
both would fight over a single Cargo target volume across different branches.

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
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator

IMAGE = "soldr-cook-dev"
DOCKERFILE = "docker/cook-shared-cache/Dockerfile"
DOCKER_CONTEXT = "docker/cook-shared-cache"
CONTAINER_PREFIX = "soldr-perf-local"
VOLUME_PREFIX = "soldr-perf"
# Bumped from "1": schema 1 runners are the pre-per-root shared containers.
RUNNER_SCHEMA = "2"
LABEL_PREFIX = "io.soldr.perf-local"
PTRACE_ENV = "SOLDR_PERF_LOCAL_PTRACE"

# The global names this script used before per-root isolation. These are NOT
# dead: `bench/cook_in_docker.sh` mounts soldr-perf-target and
# soldr-perf-cargo-home directly, and sibling checkouts still on the old
# script drive all of them. Nothing here ever removes them — `--status` only
# reports them so their disk is not mistaken for this runner's.
SHARED_CONTAINER = "soldr-perf-local"
SHARED_VOLUMES = ("soldr-perf-target", "soldr-perf-cargo-home", "soldr-perf-soldr-home")

USAGE = """\
usage: python ci/perf_local.py cargo <args...>
       python ci/perf_local.py --status
       python ci/perf_local.py --stop
       python ci/perf_local.py --reset-runner
       python ci/perf_local.py --wipe

Every argument after `cargo` is forwarded verbatim to Cargo in the recycled
runner. --stop and --reset-runner preserve named volumes; --wipe removes them.

The runner and its volumes are per-checkout-root, so sibling checkouts
(soldr, soldr2, soldr3) never share or evict each other's. --status prints
which root the current runner belongs to.
"""


@dataclass(frozen=True)
class Runner:
    """Docker resource names for one shared git root."""

    source_root: Path
    container: str
    target: str
    cargo_home: str
    soldr_home: str

    @property
    def volumes(self) -> tuple[str, str, str]:
        return (self.target, self.cargo_home, self.soldr_home)


def root_slug(source_root: Path) -> str:
    """Readable, docker-safe fragment of the root's directory name."""
    raw = source_root.resolve().name.lower()
    cleaned = "".join(char if char.isalnum() else "-" for char in raw).strip("-")
    return cleaned[:24] or "repo"


def root_tag(source_root: Path) -> str:
    """Stable per-root suffix.

    Case-folded because Windows paths are case-insensitive: `C:\\...\\Soldr2`
    and `c:\\...\\soldr2` are the same checkout and must not get two runners.
    """
    canonical = str(source_root.resolve()).casefold().encode("utf-8")
    return hashlib.sha256(canonical).hexdigest()[:8]


def runner_for(source_root: Path) -> Runner:
    suffix = f"{root_slug(source_root)}-{root_tag(source_root)}"
    return Runner(
        source_root=source_root,
        container=f"{CONTAINER_PREFIX}-{suffix}",
        target=f"{VOLUME_PREFIX}-target-{suffix}",
        cargo_home=f"{VOLUME_PREFIX}-cargo-home-{suffix}",
        soldr_home=f"{VOLUME_PREFIX}-soldr-home-{suffix}",
    )


def main(argv: list[str]) -> int:
    repo_root = Path(__file__).resolve().parent.parent
    source_root = shared_source_root(repo_root)
    runner = runner_for(source_root)
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
        return status(runner)
    if argv[0] in ("--stop", "--reset-runner", "--wipe"):
        with runner_lock(source_root):
            if argv[0] == "--stop":
                return stop_runner(runner)
            if argv[0] == "--reset-runner":
                return reset_runner(runner)
            return wipe(runner)
    if argv[0] != "cargo":
        print(
            f"error: expected `cargo` as the first arg, got {argv[0]!r}\n\n{USAGE}",
            file=sys.stderr,
        )
        return 2

    # Serialize the full command. Cargo can lock a shared target itself, but
    # parallel LTO builds make every sample slower and invalidate perf data.
    # The lock is per-root, which is only safe because the runner and volumes
    # it guards are per-root too.
    with runner_lock(source_root):
        try:
            image_id = ensure_image(repo_root)
        except RuntimeError as error:
            print(f"error: {error}", file=sys.stderr)
            return 1
        ensure_runner(runner, image_id)
        workdir = container_workdir(source_root, repo_root)
        completed = subprocess.run(
            exec_command(runner, argv, workdir, tty=tty_enabled()), check=False
        )
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


def create_command(runner: Runner, image_id: str) -> list[str]:
    command = ["docker", "create", "--name", runner.container, "--init"]
    if ptrace_enabled():
        command.extend(["--cap-add=SYS_PTRACE", "--security-opt", "seccomp=unconfined"])
    for key, value in expected_labels(runner.source_root, image_id).items():
        command.extend(["--label", f"{key}={value}"])
    command.extend(
        [
            "-v",
            f"{runner.source_root.resolve()}:/repo",
            "-v",
            f"{runner.soldr_home}:/root/.soldr",
            "-v",
            f"{runner.target}:/target",
            "-v",
            f"{runner.cargo_home}:/root/.cargo",
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


def exec_command(runner: Runner, argv: list[str], workdir: str, *, tty: bool) -> list[str]:
    command = ["docker", "exec"]
    if tty:
        command.append("-it")
    command.extend(["-w", workdir, runner.container, *argv])
    return command


def ensure_runner(runner: Runner, image_id: str) -> None:
    labels = expected_labels(runner.source_root, image_id)
    inspected = subprocess.run(
        ["docker", "inspect", runner.container],
        capture_output=True,
        text=True,
        check=False,
    )
    exists = inspected.returncode == 0
    if exists:
        info = json.loads(inspected.stdout)[0]
        # A name collision is now impossible across roots, so a mismatch only
        # means a stale image or schema for THIS root — safe to recreate.
        if not runner_matches(info, labels):
            subprocess.run(["docker", "rm", "-f", runner.container], check=True)
            exists = False
    if not exists:
        for volume in runner.volumes:
            subprocess.run(["docker", "volume", "create", volume], check=True)
        subprocess.run(create_command(runner, image_id), check=True)

    running = docker_output(["inspect", "--format", "{{.State.Running}}", runner.container])
    if running != "true":
        subprocess.run(["docker", "start", runner.container], check=True)


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


def reset_runner(runner: Runner) -> int:
    if not docker_output(["inspect", "--format", "{{.Id}}", runner.container]):
        return 0
    return subprocess.run(["docker", "rm", "-f", runner.container], check=False).returncode


def stop_runner(runner: Runner) -> int:
    if not docker_output(["inspect", "--format", "{{.Id}}", runner.container]):
        return 0
    return subprocess.run(["docker", "stop", runner.container], check=False).returncode


def wipe(runner: Runner) -> int:
    reset_runner(runner)
    out = subprocess.run(
        ["docker", "volume", "rm", "--force", *runner.volumes],
        check=False,
    )
    return out.returncode


def status(runner: Runner) -> int:
    state = (
        docker_output(["inspect", "--format", "{{.State.Status}}", runner.container]) or "(absent)"
    )
    print(f"root: {runner.source_root}")
    print(f"{runner.container}  {state}")
    rows: list[tuple[str, str]] = []
    for name in runner.volumes:
        mountpoint = docker_output(["volume", "inspect", "--format", "{{.Mountpoint}}", name])
        rows.append((name, mountpoint or "(absent)"))
    width = max(len(name) for name, _ in rows)
    for name, info in rows:
        print(f"{name:<{width}}  {info}")
    report_shared_resources()
    return 0


def report_shared_resources() -> None:
    """Report the non-per-root resources, which this runner no longer uses."""
    present = []
    if docker_output(["inspect", "--format", "{{.Id}}", SHARED_CONTAINER]):
        present.append(f"container {SHARED_CONTAINER}")
    for name in SHARED_VOLUMES:
        if docker_output(["volume", "inspect", "--format", "{{.Name}}", name]):
            present.append(f"volume {name}")
    if not present:
        return
    print()
    print("machine-wide resources (NOT owned by this root's runner):")
    for item in present:
        print(f"  {item}")
    print("  Still in use by bench/cook_in_docker.sh and by sibling checkouts")
    print("  running the pre-per-root script. Do not remove blindly.")


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
