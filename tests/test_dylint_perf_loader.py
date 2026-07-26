"""Guard bench/dylint_perf.py's import of ci/perf_local.py.

dylint_perf.py loads perf_local by path. A module loaded that way must be
registered in sys.modules *before* exec_module, because @dataclass resolves
annotations through sys.modules[cls.__module__]. perf_local grew a frozen
`Runner` dataclass in #1835, which turned an unregistered import into

    AttributeError: 'NoneType' object has no attribute '__dict__'

and broke the benchmark harness on main. The unit tests for perf_local were
updated at the time; this loader was missed, so pin it separately.
"""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from pathlib import Path


def load_dylint_perf():
    path = Path(__file__).parents[1] / "bench" / "dylint_perf.py"
    spec = importlib.util.spec_from_file_location("dylint_perf_under_test", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_load_perf_local_survives_dataclass_annotation_resolution() -> None:
    dylint_perf = load_dylint_perf()
    perf_local = dylint_perf.load_perf_local()

    # The dataclass is the thing that actually needs sys.modules registration.
    assert hasattr(perf_local, "Runner")
    runner = perf_local.runner_for(Path.cwd())
    assert runner.container.startswith(perf_local.CONTAINER_PREFIX)


def test_docker_runner_targets_the_per_root_container(monkeypatch) -> None:
    """DockerRunner must resolve the container the way perf_local names it.

    #1835 made runners per-checkout-root and dropped the module-level
    `CONTAINER` constant, but dylint_perf.py kept reading `perf_local.CONTAINER`
    -- an AttributeError the moment any command ran. Intercept subprocess.run
    so the argv `_exec` really builds is asserted, and a rename on either side
    fails here instead of at benchmark time.
    """
    dylint_perf = load_dylint_perf()
    perf_local = dylint_perf.load_perf_local()

    # The real repo root, because container_workdir() resolves the fixture
    # directory relative to it.
    source_root = Path(dylint_perf.REPO_ROOT)
    runner = dylint_perf.DockerRunner(perf_local, source_root)
    expected = perf_local.runner_for(source_root).container

    seen: list[list[str]] = []

    def fake_run(command, **kwargs):
        seen.append(command)
        return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

    monkeypatch.setattr(dylint_perf.subprocess, "run", fake_run)
    runner.run(["soldr", "cargo", "dylint"])

    assert len(seen) == 1
    command = seen[0]
    assert command[:2] == ["docker", "exec"]
    assert expected in command, f"container {expected!r} missing from {command!r}"
    # `docker exec ... -w <workdir> <container> <argv...>`
    assert command[command.index(expected) - 1] == runner.container_fixture_dir
    assert command[command.index(expected) + 1 :] == ["soldr", "cargo", "dylint"]
