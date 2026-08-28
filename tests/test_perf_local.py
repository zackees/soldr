from __future__ import annotations

import re
import subprocess
from pathlib import Path

import pytest

from conftest import load_script_module


def load_module():
    path = Path(__file__).parents[1] / "ci" / "perf_local.py"
    return load_script_module(path, "perf_local")


perf_local = load_module()


def test_docker_context_is_small_because_image_copies_no_source() -> None:
    assert perf_local.DOCKER_CONTEXT == "docker/cook-shared-cache"


def test_docker_image_bootstraps_with_amalgamation_safe_published_soldr() -> None:
    dockerfile = (Path(__file__).parents[1] / perf_local.DOCKERFILE).read_text(encoding="utf-8")

    bootstrap = re.search(r"^ARG SOLDR_BOOTSTRAP_VERSION=(\d+)\.(\d+)\.(\d+)$", dockerfile, re.M)
    assert bootstrap, "Docker bootstrap version must remain an explicit published SemVer pin"
    version = tuple(map(int, bootstrap.groups()))
    assert version >= (0, 9, 6), (
        "Soldr 0.9.6 is the minimum bootstrap carrying zccache's "
        "amalgamation-exclusive admission gate; older bootstraps can OOM an 8 GiB runner"
    )
    assert version == (
        0,
        9,
        10,
    ), "keep the bootstrap on the repository's current published release"
    assert '"soldr==${SOLDR_BOOTSTRAP_VERSION}"' in dockerfile
    assert "/opt/soldr-bootstrap/bin/soldr --version" in dockerfile
    assert (
        'ENV SOLDR_CACHE_DIR="/root/.soldr/bootstrap-v${SOLDR_BOOTSTRAP_VERSION}"' in dockerfile
    ), (
        "The persistent bootstrap cache/runtime root must be scoped by the explicit "
        "published bootstrap version, so a retained older daemon cannot service it."
    )
    for profile in (
        "DEV",
        "TEST",
        "RELEASE",
        "BENCH",
        "CI_BOOTSTRAP",
        "CI_RELEASE",
        "CI_NEXTEST",
    ):
        assert f"CARGO_PROFILE_{profile}_OPT_LEVEL=0" in dockerfile
        assert f"CARGO_PROFILE_{profile}_LTO=false" in dockerfile
        assert f"CARGO_PROFILE_{profile}_CODEGEN_UNITS=256" in dockerfile
        assert f"CARGO_PROFILE_{profile}_INCREMENTAL=true" in dockerfile
        assert f"CARGO_PROFILE_{profile}_DEBUG=0" in dockerfile


def test_bosn_workspace_test_hands_off_from_bootstrap_to_source() -> None:
    handoff = load_script_module(
        Path(__file__).parents[1] / "ci" / "bosn_workspace_test.py",
        "bosn_workspace_test",
    )

    plan = handoff.workspace_test_plan(
        repo=Path("/repo"),
        target=Path("/target"),
        bootstrap=Path("/opt/soldr-bootstrap/bin/soldr"),
    )

    assert [step.argv for step in plan] == [
        ["/opt/soldr-bootstrap/bin/soldr", "cargo", "build", "-p", "soldr-cli", "--bin", "soldr"],
        ["/opt/soldr-bootstrap/bin/soldr", "cache", "shutdown", "--shutdown-timeout-seconds", "30"],
        ["/opt/soldr-bootstrap/bin/soldr", "broker", "remove"],
        ["/target/debug/soldr", "daemon", "start"],
        ["/target/debug/soldr", "cargo", "test", "--workspace"],
        ["/target/debug/soldr", "cache", "shutdown", "--shutdown-timeout-seconds", "30"],
        ["/target/debug/soldr", "broker", "remove"],
    ]
    validation = plan[4]
    assert validation.env["SOLDR_RUSTC_WRAPPER"] == "/target/debug/soldr"
    assert validation.env["CARGO_TARGET_DIR"] == "/target"


def test_bosn_workspace_test_cleans_up_source_route_after_validation_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    handoff = load_script_module(
        Path(__file__).parents[1] / "ci" / "bosn_workspace_test.py",
        "bosn_workspace_test_failure",
    )
    plan = handoff.workspace_test_plan(
        repo=Path("/repo"),
        target=Path("/target"),
        bootstrap=Path("/opt/soldr-bootstrap/bin/soldr"),
    )
    calls: list[list[str]] = []

    def fake_run(step: object, *, repo: Path) -> None:
        argv = step.argv
        calls.append(argv)
        if argv == plan[4].argv:
            raise subprocess.CalledProcessError(101, argv)

    monkeypatch.setattr(handoff, "workspace_test_plan", lambda **_: plan)
    monkeypatch.setattr(handoff, "run_step", fake_run)

    with pytest.raises(subprocess.CalledProcessError) as error:
        handoff.main([])

    assert error.value.cmd == plan[4].argv
    assert calls == [step.argv for step in plan]


def test_bosn_workspace_test_cleans_up_source_route_after_start_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    handoff = load_script_module(
        Path(__file__).parents[1] / "ci" / "bosn_workspace_test.py",
        "bosn_workspace_test_start_failure",
    )
    plan = handoff.workspace_test_plan(
        repo=Path("/repo"),
        target=Path("/target"),
        bootstrap=Path("/opt/soldr-bootstrap/bin/soldr"),
    )
    calls: list[list[str]] = []

    def fake_run(step: object, *, repo: Path) -> None:
        argv = step.argv
        calls.append(argv)
        if argv == plan[3].argv:
            raise subprocess.CalledProcessError(101, argv)

    monkeypatch.setattr(handoff, "workspace_test_plan", lambda **_: plan)
    monkeypatch.setattr(handoff, "run_step", fake_run)

    with pytest.raises(subprocess.CalledProcessError) as error:
        handoff.main([])

    assert error.value.cmd == plan[3].argv
    assert calls == [step.argv for step in plan[:4]] + [step.argv for step in plan[5:]]


def test_bosn_workspace_test_preserves_validation_error_when_cleanup_fails(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    handoff = load_script_module(
        Path(__file__).parents[1] / "ci" / "bosn_workspace_test.py",
        "bosn_workspace_test_cleanup_failure",
    )
    plan = handoff.workspace_test_plan(
        repo=Path("/repo"),
        target=Path("/target"),
        bootstrap=Path("/opt/soldr-bootstrap/bin/soldr"),
    )
    calls: list[list[str]] = []

    def fake_run(step: object, *, repo: Path) -> None:
        argv = step.argv
        calls.append(argv)
        if argv in (plan[4].argv, plan[5].argv):
            raise subprocess.CalledProcessError(101, argv)

    monkeypatch.setattr(handoff, "workspace_test_plan", lambda **_: plan)
    monkeypatch.setattr(handoff, "run_step", fake_run)

    with pytest.raises(subprocess.CalledProcessError) as error:
        handoff.main([])

    assert error.value.cmd == plan[4].argv
    assert calls == [step.argv for step in plan]


def test_dockerfile_digest_changes_with_content(tmp_path: Path) -> None:
    dockerfile = tmp_path / perf_local.DOCKERFILE
    dockerfile.parent.mkdir(parents=True)
    dockerfile.write_text("FROM scratch\n", encoding="utf-8")
    first = perf_local.dockerfile_digest(tmp_path)
    dockerfile.write_text("FROM scratch\nLABEL changed=1\n", encoding="utf-8")
    second = perf_local.dockerfile_digest(tmp_path)
    assert first != second


def test_incremental_gc_selects_only_the_oldest_stale_runner(tmp_path: Path) -> None:
    current = tmp_path / "current"
    current.mkdir()
    old = tmp_path / "old"
    old.mkdir()
    oldest = tmp_path / "oldest"
    oldest.mkdir()
    candidates = [
        {"source_root": current, "last_used_epoch": 0.0},
        {"source_root": old, "last_used_epoch": 20.0},
        {"source_root": oldest, "last_used_epoch": 10.0},
    ]

    selected = perf_local.incremental_gc_candidate(
        candidates, current_root=current, now_epoch=100.0, max_age_secs=50.0
    )

    assert selected == candidates[2]


def test_incremental_gc_prioritizes_a_missing_checkout(tmp_path: Path) -> None:
    current = tmp_path / "current"
    current.mkdir()
    present = tmp_path / "present"
    present.mkdir()
    missing = tmp_path / "missing"
    candidates = [
        {"source_root": present, "last_used_epoch": 1.0},
        {"source_root": missing, "last_used_epoch": 99.0},
    ]

    selected = perf_local.incremental_gc_candidate(
        candidates, current_root=current, now_epoch=100.0, max_age_secs=50.0
    )

    assert selected == candidates[1]


def test_incremental_gc_never_selects_the_current_runner(tmp_path: Path) -> None:
    current = tmp_path / "current"
    current.mkdir()
    assert (
        perf_local.incremental_gc_candidate(
            [{"source_root": current, "last_used_epoch": 0.0}],
            current_root=current,
            now_epoch=100.0,
            max_age_secs=1.0,
        )
        is None
    )


def test_incremental_gc_fast_triggers_above_group_limit(tmp_path: Path) -> None:
    current = tmp_path / "current"
    current.mkdir()
    candidates = [{"source_root": current, "last_used_epoch": 99.0}]
    for index in range(perf_local.MAX_RUNNER_GROUPS):
        root = tmp_path / f"runner-{index}"
        root.mkdir()
        candidates.append({"source_root": root, "last_used_epoch": float(index + 1)})
    selected = perf_local.incremental_gc_candidate(
        candidates,
        current_root=current,
        now_epoch=100.0,
        max_age_secs=perf_local.GC_MAX_AGE_SECS,
    )
    assert selected == candidates[1]


def test_buildkit_prune_is_scoped_to_soldr_builder() -> None:
    assert perf_local.buildkit_prune_command() == [
        "docker",
        "buildx",
        "prune",
        "--builder",
        perf_local.BUILDER_NAME,
        "--filter",
        "until=24h",
        "--force",
    ]


def test_runner_storage_budget_is_a_hard_ceiling() -> None:
    assert not perf_local.runner_over_budget(perf_local.RUNNER_VOLUME_BUDGET_BYTES)
    assert perf_local.runner_over_budget(perf_local.RUNNER_VOLUME_BUDGET_BYTES + 1)


def test_activity_marker_lives_in_soldr_state_not_the_checkout(tmp_path: Path, monkeypatch) -> None:
    state = tmp_path / "state"
    checkout = tmp_path / "checkout"
    monkeypatch.setattr(perf_local, "GC_STATE_DIR", state)
    marker = perf_local.activity_marker(checkout)
    assert marker.parent == state
    assert checkout not in marker.parents


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
    assert f"{runner.uv_cache}:/root/.cache/uv" in command
    assert f"{runner.venv}:/venv" in command
    assert "CARGO_TARGET_DIR=/target" in command
    assert "TMPDIR=/target/tmp" in command
    assert "UV_PROJECT_ENVIRONMENT=/venv" in command
    assert "NEXTEST_TEST_THREADS=2" in command
    assert "CARGO_BUILD_JOBS=2" in command
    assert "SOLDR_JOBS=2" in command
    assert command[-3:] == ["tail", "-f", "/dev/null"]


def test_create_command_enables_ptrace_only_when_requested(tmp_path: Path, monkeypatch) -> None:
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

    stale = {"Config": {"Labels": {**labels, f"{perf_local.LABEL_PREFIX}.image-id": "old"}}}
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
    assert perf_local.exec_command(runner, ["cargo", "check"], "/repo", tty=True)[:3] == [
        "docker",
        "exec",
        "-it",
    ]


def test_smoke_debug_traces_and_retains_timelines() -> None:
    # soldr#2546: recursive Docker smoke runs can retain the process JSONL.
    assert perf_local.container_argv(["smoke-debug"]) == [
        "env",
        "SOLDR_DEBUG_TRACE=1",
        "bash",
        "ci/smoke_local.sh",
    ]
    assert callable(perf_local.retain_debug_trace)


def test_smoke_command_runs_the_complete_repository_pipeline() -> None:
    assert perf_local.container_argv(["smoke"]) == ["bash", "ci/smoke_local.sh"]
    assert perf_local.container_argv(["smoke-console"]) == [
        "env",
        "SOLDR_SMOKE_TOKIO_CONSOLE=1",
        "bash",
        "ci/smoke_local.sh",
    ]
    assert perf_local.container_argv(["cargo", "check"]) == ["cargo", "check"]
