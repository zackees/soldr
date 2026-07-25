from __future__ import annotations

import importlib.util
from pathlib import Path


def load_module():
    path = Path(__file__).parents[1] / "ci" / "perf_local.py"
    spec = importlib.util.spec_from_file_location("perf_local", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


perf_local = load_module()


def test_docker_context_is_small_because_image_copies_no_source() -> None:
    assert perf_local.DOCKER_CONTEXT == "docker/cook-shared-cache"


def test_dockerfile_digest_changes_with_content(tmp_path: Path) -> None:
    dockerfile = tmp_path / perf_local.DOCKERFILE
    dockerfile.parent.mkdir(parents=True)
    dockerfile.write_text("FROM scratch\n", encoding="utf-8")
    first = perf_local.dockerfile_digest(tmp_path)
    dockerfile.write_text("FROM scratch\nLABEL changed=1\n", encoding="utf-8")
    second = perf_local.dockerfile_digest(tmp_path)
    assert first != second


def test_container_workdir_supports_shared_root_and_nested_worktree(tmp_path: Path) -> None:
    root = tmp_path / "soldr"
    worktree = root / ".claude" / "issue-1553"
    worktree.mkdir(parents=True)

    assert perf_local.container_workdir(root, root) == "/repo"
    assert perf_local.container_workdir(root, worktree) == "/repo/.claude/issue-1553"


def test_create_command_uses_one_named_runner_and_persistent_volumes(tmp_path: Path) -> None:
    command = perf_local.create_command(tmp_path, "sha256:image")

    assert command[:4] == ["docker", "create", "--name", "soldr-perf-local"]
    assert "--init" in command
    assert f"{tmp_path.resolve()}:/repo" in command
    assert "soldr-perf-target:/target" in command
    assert "soldr-perf-cargo-home:/root/.cargo" in command
    assert "soldr-perf-soldr-home:/root/.soldr" in command
    assert "CARGO_TARGET_DIR=/target" in command
    assert command[-3:] == ["tail", "-f", "/dev/null"]


def test_create_command_enables_ptrace_only_when_requested(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.setenv(perf_local.PTRACE_ENV, "1")
    command = perf_local.create_command(tmp_path, "sha256:image")

    assert "--cap-add=SYS_PTRACE" in command
    assert ["--security-opt", "seccomp=unconfined"] == command[
        command.index("--security-opt") : command.index("--security-opt") + 2
    ]
    assert f"{perf_local.LABEL_PREFIX}.ptrace=1" in command

    monkeypatch.delenv(perf_local.PTRACE_ENV)
    command = perf_local.create_command(tmp_path, "sha256:image")
    assert "--cap-add=SYS_PTRACE" not in command
    assert f"{perf_local.LABEL_PREFIX}.ptrace=0" in command


def test_runner_match_requires_schema_image_and_source_root(tmp_path: Path) -> None:
    labels = perf_local.expected_labels(tmp_path, "sha256:new")
    info = {"Config": {"Labels": dict(labels)}}
    assert perf_local.runner_matches(info, labels)

    stale = {"Config": {"Labels": {**labels, f"{perf_local.LABEL_PREFIX}.image-id": "old"}}}
    assert not perf_local.runner_matches(stale, labels)
    assert not perf_local.runner_matches({"Config": {"Labels": None}}, labels)


def test_exec_command_reuses_runner_and_changes_only_workdir() -> None:
    assert perf_local.exec_command(
        ["cargo", "test", "--workspace"], "/repo/.claude/issue-1", tty=False
    ) == [
        "docker",
        "exec",
        "-w",
        "/repo/.claude/issue-1",
        "soldr-perf-local",
        "cargo",
        "test",
        "--workspace",
    ]
    assert perf_local.exec_command(["cargo", "check"], "/repo", tty=True)[:3] == [
        "docker",
        "exec",
        "-it",
    ]
