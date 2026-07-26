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
    # `docker exec ... -w <workdir> <container> sh -c '<path prefix>' sh <argv...>`
    at = command.index(expected)
    assert command[at - 1] == runner.container_fixture_dir
    # The argv survives the PATH wrapper unmodified, at the tail.
    assert command[-3:] == ["soldr", "cargo", "dylint"]
    # ...and the wrapper is what puts the container-built soldr on PATH,
    # which is the whole reason `soldr cargo dylint` resolves at all.
    assert any(dylint_perf.SOLDR_BIN_DIR in part for part in command[at:])


def test_main_drives_a_full_docker_bench_without_api_drift(monkeypatch) -> None:
    """Run main() with only the subprocess layer faked.

    The two call-site regressions above were each found by *running* the
    benchmark, not by a test -- and a third (`ensure_runner` taking a `Runner`
    rather than a source-root `Path` since #1835) survived the first repair
    because the tests only poked at individual helpers.

    So exercise the real wiring: everything in perf_local runs for real, and
    only `subprocess.run` and `ensure_image` are stubbed. Any future signature
    drift across the dylint_perf <-> perf_local boundary fails here.
    """
    dylint_perf = load_dylint_perf()
    perf_local = dylint_perf.load_perf_local()

    def fake_run(command, **kwargs):
        # `docker inspect <container>` with no --format is the existence probe;
        # a non-zero exit means "absent", which sends ensure_runner down the
        # create-volumes-and-container path. Returning 0 with empty stdout
        # instead would make it json.loads("").
        if "inspect" in command and "--format" not in command:
            return subprocess.CompletedProcess(command, 1, stdout="", stderr="")
        return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

    monkeypatch.setattr(dylint_perf.shutil, "which", lambda _name: "/usr/bin/docker")
    monkeypatch.setattr(perf_local, "ensure_image", lambda _root: "sha256:test-image")
    monkeypatch.setattr(perf_local.subprocess, "run", fake_run)
    monkeypatch.setattr(dylint_perf.subprocess, "run", fake_run)
    # load_perf_local() is called inside main(); hand back the instance we
    # already patched rather than a pristine second import.
    monkeypatch.setattr(dylint_perf, "load_perf_local", lambda: perf_local)

    assert dylint_perf.main([]) == 0
