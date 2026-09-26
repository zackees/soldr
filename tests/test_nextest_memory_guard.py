"""soldr#2885: memory-aware admission and memory-failure isolation in the
Nextest wrapper.

Every Unix test runs through ``.github/scripts/nextest_timeout_wrapper.py``.
``soldr ci-test`` hands the wrapper an admission directory, a per-test memory
ceiling and a one-line admission summary. These tests drive the real wrapper
as Nextest does -- ``wrapper <test-binary> <args>`` -- with bounded-memory
fixtures, so they pin observable behaviour rather than helper internals.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
WRAPPER = REPO_ROOT / ".github" / "scripts" / "nextest_timeout_wrapper.py"
guard = load_script_module(WRAPPER.parent / "nextest_memory_guard.py")

MIB = 1024 * 1024
INFRA_EXIT = 75
POSIX = pytest.mark.skipif(
    os.name != "posix", reason="the wrapper runs Unix tests only"
)
LINUX = pytest.mark.skipif(
    not sys.platform.startswith("linux"), reason="exercises Linux kernel limits"
)

# Touches every page, so the allocation is resident rather than lazily mapped.
_HOG = """
import sys, time
hog = b"\\x01" * (int(sys.argv[1]) * 1024 * 1024)
print("allocated", flush=True)
time.sleep(float(sys.argv[2]))
"""


def _env(tmp_path: Path, **extra: str) -> dict[str, str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("SOLDR_NEXTEST_") and not key.startswith("NEXTEST_")
    }
    env.update(
        {
            "TMPDIR": str(tmp_path),
            "NEXTEST_BINARY_ID": "soldr-cli::fixture",
            "NEXTEST_TEST_NAME": "memory::fixture_test",
        }
    )
    env.update(extra)
    return env


def _run(
    args: list[str], env: dict[str, str], timeout: float = 60, **kwargs
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(WRAPPER), sys.executable, *args],
        capture_output=True,
        text=True,
        env=env,
        timeout=timeout,
        check=False,
        **kwargs,
    )


def _admission_dir(tmp_path: Path) -> Path:
    directory = tmp_path / "admission"
    (directory / "active").mkdir(parents=True)
    return directory


# ---------------------------------------------------------------------------
# RED -> GREEN: an over-budget test tree fails alone, with a named diagnostic.
# ---------------------------------------------------------------------------


@POSIX
def test_over_ceiling_test_tree_is_killed_with_a_named_memory_diagnostic(
    tmp_path: Path,
) -> None:
    admission = _admission_dir(tmp_path)
    env = _env(
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES=str(96 * MIB),
        SOLDR_NEXTEST_ADMISSION_SUMMARY=(
            "requested NEXTEST_TEST_THREADS=unset, effective=4 (measured)"
        ),
    )
    started = time.monotonic()
    result = _run(["-c", _HOG, "400", "30"], env)
    elapsed = time.monotonic() - started

    assert result.returncode == INFRA_EXIT, result.stderr
    assert elapsed < 20, "the ceiling must stop the tree, not wait for the test"
    stderr = result.stderr
    assert "nextest memory: infrastructure failure, not a test assertion" in stderr
    assert "test: soldr-cli::fixture memory::fixture_test" in stderr
    assert "per-test memory ceiling 96.0 MiB" in stderr
    assert "process-tree peak RSS" in stderr
    assert "requested NEXTEST_TEST_THREADS=unset, effective=4 (measured)" in stderr
    assert "memory available at admission" in stderr
    assert "memory available at failure" in stderr
    assert "pid pressure" in stderr
    # ci-test lists every infrastructure failure after Nextest exits.
    recorded = [path.name for path in (admission / "infra").iterdir()]
    assert recorded == [
        guard.infra_record_name("soldr-cli::fixture memory::fixture_test")
    ]


@POSIX
def test_ordinary_assertion_failure_is_not_labelled_a_memory_failure(
    tmp_path: Path,
) -> None:
    admission = _admission_dir(tmp_path)
    env = _env(
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES=str(512 * MIB),
    )
    child = "import sys; print('assertion failed: left == right', file=sys.stderr); sys.exit(101)"
    result = _run(["-c", child], env)

    assert result.returncode == 101
    assert "nextest memory:" not in result.stderr
    assert not (admission / "infra").exists()


@POSIX
def test_well_behaved_neighbour_is_untouched_while_a_hog_is_killed(
    tmp_path: Path,
) -> None:
    admission = _admission_dir(tmp_path)
    env = _env(
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES=str(96 * MIB),
    )
    results: dict[str, subprocess.CompletedProcess[str]] = {}

    def run(name: str, megabytes: str) -> None:
        results[name] = _run(
            ["-c", _HOG, megabytes, "2"],
            {**env, "NEXTEST_TEST_NAME": f"memory::{name}"},
        )

    threads = [
        threading.Thread(target=run, args=("hog", "400")),
        threading.Thread(target=run, args=("neighbour", "8")),
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert results["hog"].returncode == INFRA_EXIT, results["hog"].stderr
    assert results["neighbour"].returncode == 0, results["neighbour"].stderr
    assert "nextest memory:" not in results["neighbour"].stderr
    assert [path.name for path in (admission / "infra").iterdir()] == [
        guard.infra_record_name("soldr-cli::fixture memory::hog")
    ]


@LINUX
def test_enomem_inside_the_test_is_classified_as_infrastructure(
    tmp_path: Path,
) -> None:
    """The soldr#2885 shape: an allocation fails with ENOMEM mid-test."""

    admission = _admission_dir(tmp_path)
    env = _env(tmp_path, SOLDR_NEXTEST_ADMISSION_DIR=str(admission))
    child = (
        "import resource\n"
        "resource.setrlimit(resource.RLIMIT_AS, (512 * 1024 * 1024,) * 2)\n"
        "hog = bytearray(2048 * 1024 * 1024)\n"
    )
    result = _run(["-c", child], env)

    assert result.returncode == INFRA_EXIT, result.stderr
    assert "MemoryError" in result.stderr, "the test's own output is preserved"
    assert (
        "nextest memory: infrastructure failure, not a test assertion" in result.stderr
    )
    assert "memory-exhaustion signature" in result.stderr
    assert "original exit status: 1" in result.stderr


@LINUX
def test_process_start_failure_is_named_as_pid_or_memory_pressure(
    tmp_path: Path,
) -> None:
    if os.geteuid() == 0:
        pytest.skip("RLIMIT_NPROC does not bind root")
    import resource  # pylint: disable=import-outside-toplevel  # Unix-only module

    env = _env(tmp_path)

    def no_more_processes() -> None:
        resource.setrlimit(resource.RLIMIT_NPROC, (0, 0))

    result = _run(
        ["-c", "pass"],
        env,
        preexec_fn=no_more_processes,  # pylint: disable=subprocess-popen-preexec-fn
    )

    assert result.returncode == INFRA_EXIT, result.stderr
    assert (
        "nextest memory: infrastructure failure, not a test assertion" in result.stderr
    )
    assert "could not start the test process" in result.stderr
    assert "Traceback" not in result.stderr


# ---------------------------------------------------------------------------
# Admission gate: soldr ci-test's controller pauses and resumes admissions.
# ---------------------------------------------------------------------------


def _live_sleeper() -> subprocess.Popen[bytes]:
    return subprocess.Popen(  # pylint: disable=consider-using-with
        [sys.executable, "-c", "import time; time.sleep(60)"]
    )


@POSIX
def test_paused_admission_holds_a_new_test_until_pressure_clears(
    tmp_path: Path,
) -> None:
    admission = _admission_dir(tmp_path)
    sleeper = _live_sleeper()
    try:
        (admission / "active" / str(sleeper.pid)).touch()
        (admission / guard.PAUSED_FLAG).touch()
        env = _env(
            tmp_path,
            SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
            SOLDR_NEXTEST_ADMISSION_MAX_WAIT_SECS="30",
        )
        cleared_at: list[float] = []

        def clear_later() -> None:
            time.sleep(1.5)
            cleared_at.append(time.time())
            (admission / guard.PAUSED_FLAG).unlink()

        clearer = threading.Thread(target=clear_later)
        clearer.start()
        result = _run(["-c", "import time; print(time.time())"], env)
        clearer.join()
    finally:
        sleeper.kill()
        sleeper.wait()

    assert result.returncode == 0, result.stderr
    started = float(result.stdout.split()[0])
    assert started >= cleared_at[0], "the test started while admission was paused"
    assert "admission paused by memory pressure" in result.stderr


@POSIX
def test_paused_admission_is_bounded_and_says_so(tmp_path: Path) -> None:
    admission = _admission_dir(tmp_path)
    sleeper = _live_sleeper()
    try:
        (admission / "active" / str(sleeper.pid)).touch()
        (admission / guard.PAUSED_FLAG).touch()
        env = _env(
            tmp_path,
            SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
            SOLDR_NEXTEST_ADMISSION_MAX_WAIT_SECS="1",
        )
        started = time.monotonic()
        result = _run(["-c", "pass"], env)
        elapsed = time.monotonic() - started
    finally:
        sleeper.kill()
        sleeper.wait()

    assert result.returncode == 0, result.stderr
    assert 1.0 <= elapsed < 15
    assert "admitted after the 1s pressure wait bound" in result.stderr


@POSIX
def test_a_paused_gate_never_holds_the_only_test(tmp_path: Path) -> None:
    """With nothing else running there is nothing to drain; waiting cannot help."""

    admission = _admission_dir(tmp_path)
    # A stale slot from a SIGKILLed wrapper names a dead pid and must not count.
    (admission / "active" / "999999999").touch()
    (admission / guard.PAUSED_FLAG).touch()
    env = _env(
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_ADMISSION_MAX_WAIT_SECS="30",
    )
    started = time.monotonic()
    result = _run(["-c", "pass"], env)

    assert result.returncode == 0, result.stderr
    assert time.monotonic() - started < 10
    assert "admission paused" not in result.stderr


@POSIX
def test_active_slot_is_held_for_the_test_lifetime_and_released(
    tmp_path: Path,
) -> None:
    admission = _admission_dir(tmp_path)
    env = _env(tmp_path, SOLDR_NEXTEST_ADMISSION_DIR=str(admission))
    child = "import os, sys; print(sorted(os.listdir(sys.argv[1])))"
    result = _run(["-c", child, str(admission / "active")], env)

    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() != "[]", "the running test must hold a slot"
    assert not list((admission / "active").iterdir()), "the slot outlived the test"


@POSIX
def test_admission_controls_are_not_inherited_by_the_test_process(
    tmp_path: Path,
) -> None:
    admission = _admission_dir(tmp_path)
    env = _env(
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES=str(512 * MIB),
        SOLDR_NEXTEST_ADMISSION_SUMMARY="summary",
    )
    child = (
        "import os\n"
        "print(sorted(k for k in os.environ if k.startswith('SOLDR_NEXTEST_ADMISSION')"
        " or k == 'SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES'))"
    )
    result = _run(["-c", child], env)
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "[]"


@POSIX
def test_without_ci_test_controls_the_wrapper_behaves_as_before(tmp_path: Path) -> None:
    result = _run(["-c", _HOG, "64", "0"], _env(tmp_path))
    assert result.returncode == 0, result.stderr
    assert "nextest memory:" not in result.stderr


# ---------------------------------------------------------------------------
# Pure helpers: signature matching, tree sampling, cgroup ceiling bookkeeping.
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "text",
    [
        b"memory allocation of 1048576 bytes failed\n",
        b"OSError: [Errno 12] Cannot allocate memory: '/repo/.github/scripts'\n",
        b'Error: Os { code: 12, kind: OutOfMemory, message: "Cannot allocate memory" }',
        b"failed to spawn: Cannot allocate memory (os error 12)\n",
        b"Traceback (most recent call last):\nMemoryError\n",
    ],
)
def test_memory_exhaustion_signatures_are_recognised(text: bytes) -> None:
    assert guard.memory_signature(text) is not None


@pytest.mark.parametrize(
    "text",
    [
        b"assertion failed: left == right\n",
        b"thread 'main' panicked at src/lib.rs:1:1\n",
        b"",
    ],
)
def test_ordinary_failures_carry_no_memory_signature(text: bytes) -> None:
    assert guard.memory_signature(text) is None


def test_stderr_tail_keeps_only_the_most_recent_bytes() -> None:
    tail = guard.OutputTail(limit=8)
    tail.feed(b"0123456789")
    tail.feed(b"abc")
    assert tail.bytes() == b"56789abc"


@LINUX
def test_linux_tree_rss_counts_descendants(tmp_path: Path) -> None:
    child = subprocess.Popen(  # pylint: disable=consider-using-with
        [
            sys.executable,
            "-c",
            "import subprocess, sys, time\n"
            "grand = subprocess.Popen([sys.executable, '-c', "
            "'hog = b\"\\\\x01\" * (128 * 1024 * 1024); import time; time.sleep(30)'])\n"
            "time.sleep(30)\n",
        ]
    )
    try:
        deadline = time.monotonic() + 20
        observed = 0
        while time.monotonic() < deadline:
            sample = guard.sample_tree(child.pid)
            observed = sample.rss_bytes if sample else 0
            if observed >= 128 * MIB and sample.process_count >= 2:
                break
            time.sleep(0.1)
        assert observed >= 128 * MIB
    finally:
        guard.kill_tree(child.pid)
        child.wait()


def test_macos_ps_table_parser_builds_the_tree() -> None:
    table = guard.parse_ps_table(
        "  10     1  2048\n  11    10  1024\n  12    11   512\n  13     1  4096\n"
    )
    sample = guard.tree_from_table(10, table)
    assert sample.rss_bytes == (2048 + 1024 + 512) * 1024
    assert sample.process_count == 3


def test_cgroup_ceiling_prepares_a_leaf_and_reports_peak_and_oom(
    tmp_path: Path,
) -> None:
    root = tmp_path / "delegated"
    root.mkdir()
    (root / "cgroup.subtree_control").write_text("cpu memory pids\n")
    ceiling = guard.CgroupCeiling.create(root, 256 * MIB, owner_pid=4242)
    assert ceiling is not None
    leaf = ceiling.path
    assert leaf.parent == root
    assert (leaf / "memory.max").read_text() == str(256 * MIB)
    assert (leaf / "memory.oom.group").read_text() == "1"
    # The kernel would populate these; the fixture plays the kernel.
    (leaf / "memory.peak").write_text(f"{300 * MIB}\n")
    (leaf / "memory.events").write_text("low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\n")
    (leaf / "cgroup.procs").write_text("")
    outcome = ceiling.outcome()
    assert outcome.peak_bytes == 300 * MIB
    assert outcome.oom_kills == 1
    ceiling.release()


def test_cgroup_ceiling_pins_swap_to_zero_when_swap_is_accounted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Hosted runners have swap: without this the overflow pages out unpunished."""

    root = tmp_path / "delegated"
    root.mkdir()
    (root / "cgroup.subtree_control").write_text("memory pids\n")
    real_mkdir = Path.mkdir

    def kernel_mkdir(self: Path, *args, **kwargs) -> None:
        real_mkdir(self, *args, **kwargs)
        if self.parent == root:
            (self / "memory.swap.max").write_text("max\n")

    monkeypatch.setattr(Path, "mkdir", kernel_mkdir)
    ceiling = guard.CgroupCeiling.create(root, 256 * MIB, owner_pid=7)
    assert ceiling is not None
    assert (ceiling.path / "memory.swap.max").read_text() == "0"


def test_cgroup_ceiling_declines_a_root_without_the_memory_controller(
    tmp_path: Path,
) -> None:
    root = tmp_path / "undelegated"
    root.mkdir()
    (root / "cgroup.subtree_control").write_text("cpu pids\n")
    assert guard.CgroupCeiling.create(root, 256 * MIB, owner_pid=1) is None


def test_infra_record_names_are_safe_file_names() -> None:
    name = guard.infra_record_name("soldr-cli::guards cli/ci::test name")
    assert "/" not in name
    assert name == "soldr-cli::guards cli%2Fci::test name"
    # Non-ASCII is escaped byte-by-byte, so ci-test's percent decoder
    # (`test_pressure::percent_decode`) reassembles the exact UTF-8.
    assert guard.infra_record_name("tests::ü%") == "tests::%C3%BC%25"
    long_name = guard.infra_record_name("x" * 400)
    assert len(long_name) <= 200 and "~" in long_name


# The wrapper below runs inside a transient systemd scope that delegates the
# memory controller: it moves itself into an `init` leaf (cgroup v2 forbids
# processes in a cgroup that distributes controllers), enables memory+pids for
# the scope's children, then runs the wrapper with that scope as its root.
_DELEGATED_SCOPE = """
set -e
cg=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)
mkdir "$cg/init"
echo 0 > "$cg/init/cgroup.procs"
echo '+memory +pids' > "$cg/cgroup.subtree_control"
echo "ROOT=$cg"
set +e
SOLDR_NEXTEST_CGROUP_ROOT="$cg" "$@"
echo "EXIT=$?"
ls -d "$cg"/snt-* 2>/dev/null || echo "LEAVES=none"
"""


@LINUX
def test_delegated_cgroup_v2_ceiling_is_enforced_by_the_kernel(tmp_path: Path) -> None:
    """Linux cgroup v2: memory.max + memory.oom.group on a per-test leaf.

    Needs a cgroup the current user may delegate. A systemd user manager
    provides one on developer machines; hosted CI runners have no user bus, so
    the test skips there rather than pretending.
    """

    probe = (
        subprocess.run(
            [
                "systemd-run",
                "--user",
                "--scope",
                "--quiet",
                "-p",
                "Delegate=yes",
                "true",
            ],
            capture_output=True,
            check=False,
        )
        if shutil.which("systemd-run")
        else None
    )
    if probe is None or probe.returncode != 0:
        pytest.skip("no delegatable systemd user scope on this host")
    admission = _admission_dir(tmp_path)
    env = _env(
        tmp_path,
        SOLDR_NEXTEST_ADMISSION_DIR=str(admission),
        SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES=str(96 * MIB),
    )
    result = subprocess.run(
        [
            "systemd-run",
            "--user",
            "--scope",
            "--quiet",
            "-p",
            "Delegate=yes",
            "bash",
            "-c",
            _DELEGATED_SCOPE,
            "scope",
            sys.executable,
            str(WRAPPER),
            sys.executable,
            "-c",
            _HOG,
            "400",
            "30",
        ],
        capture_output=True,
        text=True,
        env=env,
        timeout=120,
        check=False,
    )
    assert f"EXIT={INFRA_EXIT}" in result.stdout, result.stdout + result.stderr
    assert "the kernel OOM-killed the test inside its per-test cgroup" in result.stderr
    assert "per-test memory ceiling: 96.0 MiB (cgroup v2 memory.max)" in result.stderr
    assert "(cgroup memory.peak)" in result.stderr
    assert "LEAVES=none" in result.stdout, "the per-test leaf must be removed"
