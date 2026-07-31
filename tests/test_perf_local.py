from __future__ import annotations

from pathlib import Path

from conftest import load_script_module


def load_module():
    path = Path(__file__).parents[1] / "ci" / "perf_local.py"
    return load_script_module(path, "perf_local")


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


def test_container_workdir_supports_shared_root_and_nested_worktree(
    tmp_path: Path,
) -> None:
    root = tmp_path / "soldr"
    worktree = root / ".claude" / "issue-1553"
    worktree.mkdir(parents=True)

    assert perf_local.container_workdir(root, root) == "/repo"
    assert perf_local.container_workdir(root, worktree) == "/repo/.claude/issue-1553"


def test_create_command_uses_one_named_runner_and_persistent_volumes(
    tmp_path: Path,
) -> None:
    runner = perf_local.runner_for(tmp_path)
    command = perf_local.create_command(runner, "sha256:image")

    assert command[:4] == ["docker", "create", "--name", runner.container]
    assert "--init" in command
    assert f"{tmp_path.resolve()}:/repo" in command
    assert f"{runner.target}:/target" in command
    assert f"{runner.cargo_home}:/root/.cargo" in command
    assert f"{runner.soldr_home}:/root/.soldr" in command
    assert "CARGO_TARGET_DIR=/target" in command
    assert command[-3:] == ["tail", "-f", "/dev/null"]


def test_create_command_enables_ptrace_only_when_requested(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.setenv(perf_local.PTRACE_ENV, "1")
    runner = perf_local.runner_for(tmp_path)
    command = perf_local.create_command(runner, "sha256:image")

    assert "--cap-add=SYS_PTRACE" in command
    assert ["--security-opt", "seccomp=unconfined"] == command[
        command.index("--security-opt") : command.index("--security-opt") + 2
    ]
    assert f"{perf_local.LABEL_PREFIX}.ptrace=1" in command

    monkeypatch.delenv(perf_local.PTRACE_ENV)
    command = perf_local.create_command(runner, "sha256:image")
    assert "--cap-add=SYS_PTRACE" not in command
    assert f"{perf_local.LABEL_PREFIX}.ptrace=0" in command


def test_sibling_checkouts_never_share_a_runner_or_volume(tmp_path: Path) -> None:
    """soldr / soldr2 / soldr3 must be fully isolated.

    They used to share one global `soldr-perf-local` container and one set of
    volumes while locking per-root, so a run in one checkout would `docker rm
    -f` another's running container mid-build.
    """
    roots = []
    for name in ("soldr", "soldr2", "soldr3"):
        root = tmp_path / name
        root.mkdir()
        roots.append(perf_local.runner_for(root))

    names = [r.container for r in roots]
    assert len(set(names)) == len(names), names
    for index, runner in enumerate(roots):
        others = {v for other in roots[index + 1 :] for v in other.volumes}
        assert not others.intersection(runner.volumes)


def test_runner_names_are_stable_and_docker_safe(tmp_path: Path) -> None:
    root = tmp_path / "soldr2"
    root.mkdir()

    first = perf_local.runner_for(root)
    assert first == perf_local.runner_for(root), "names must be deterministic"

    assert first.container.startswith("soldr-perf-local-")
    assert "soldr2" in first.container, "leaf name aids `docker ps`"
    for name in (first.container, *first.volumes):
        assert name[0].isalnum()
        assert all(char.isalnum() or char in "_.-" for char in name), name


def test_root_tag_is_case_insensitive_because_windows_paths_are(tmp_path: Path) -> None:
    root = tmp_path / "Soldr2"
    root.mkdir()
    lowered = Path(str(root).lower())
    assert perf_local.root_tag(root) == perf_local.root_tag(lowered)


def test_root_slug_survives_names_docker_would_reject(tmp_path: Path) -> None:
    root = tmp_path / "soldr wt #1735"
    root.mkdir()
    slug = perf_local.root_slug(root)
    assert all(char.isalnum() or char == "-" for char in slug), slug
    assert not slug.startswith("-") and not slug.endswith("-")


def test_runner_match_requires_schema_image_and_source_root(tmp_path: Path) -> None:
    labels = perf_local.expected_labels(tmp_path, "sha256:new")
    info = {"Config": {"Labels": dict(labels)}}
    assert perf_local.runner_matches(info, labels)

    stale = {
        "Config": {"Labels": {**labels, f"{perf_local.LABEL_PREFIX}.image-id": "old"}}
    }
    assert not perf_local.runner_matches(stale, labels)
    assert not perf_local.runner_matches({"Config": {"Labels": None}}, labels)


def test_exec_command_reuses_runner_and_changes_only_workdir(tmp_path: Path) -> None:
    runner = perf_local.runner_for(tmp_path)
    assert perf_local.exec_command(
        runner, ["cargo", "test", "--workspace"], "/repo/.claude/issue-1", tty=False
    ) == [
        "docker",
        "exec",
        "-w",
        "/repo/.claude/issue-1",
        runner.container,
        "cargo",
        "test",
        "--workspace",
    ]
    assert perf_local.exec_command(runner, ["cargo", "check"], "/repo", tty=True)[
        :3
    ] == [
        "docker",
        "exec",
        "-it",
    ]
