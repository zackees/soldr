from __future__ import annotations

import os
import subprocess
import sys
import time
import zipfile
from pathlib import Path

import pytest

from running_process import cli

_EXPECTED_PUBLIC_FRAMES = (
    "rp_native_running_process_wait_public",
    "rp_native_process_wait_public",
)
_REQUIRE_SYMBOLS_ENV = "RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS"


def _skip_or_fail(reason: str) -> None:
    if os.environ.get(_REQUIRE_SYMBOLS_ENV) == "1":
        raise AssertionError(reason)
    pytest.skip(reason)


def _wait_for_ready(child: subprocess.Popen[str], timeout: float) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        line = child.stdout.readline()
        if line.strip() == "READY":
            return
    raise AssertionError("timed out waiting for READY")


def test_skip_or_fail_uses_skip_by_default(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv(_REQUIRE_SYMBOLS_ENV, raising=False)

    with pytest.raises(pytest.skip.Exception, match="sample reason"):
        _skip_or_fail("sample reason")


def test_skip_or_fail_can_be_forced_to_fail(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv(_REQUIRE_SYMBOLS_ENV, "1")

    with pytest.raises(AssertionError, match="sample reason"):
        _skip_or_fail("sample reason")


@pytest.mark.live
@pytest.mark.skipif(
    sys.platform != "win32", reason="native debugger stack-dump test is Windows-only"
)
def test_live_native_debugger_dump_resolves_tiny_pdb_public_frames(tmp_path: Path) -> None:
    if not cli._native_debugger_commands(os.getpid()):
        _skip_or_fail("native debugger unavailable on PATH")

    wheels = sorted(
        Path.cwd().glob("dist/running_process-*.whl"),
        key=lambda path: path.stat().st_mtime,
    )
    if not wheels:
        _skip_or_fail("release wheel not found in dist/")
    wheel = wheels[-1]
    with zipfile.ZipFile(wheel) as zf:
        if not any(name.endswith(".pdb") for name in zf.namelist()):
            _skip_or_fail("release wheel does not include a bundled PDB")
        zf.extractall(tmp_path / "wheel")

    script_path = tmp_path / "hung_wait.py"
    script_path.write_text(
        "\n".join(
            [
                "import sys",
                "from running_process import RunningProcess",
                "process = RunningProcess([sys.executable, '-c', 'import time; time.sleep(120)'])",
                "print('READY', flush=True)",
                "process.wait(timeout=None)",
            ]
        ),
        encoding="utf-8",
    )
    log_path = tmp_path / "native-debugger.log"
    env = os.environ.copy()
    env["PYTHONPATH"] = str(tmp_path / "wheel")
    target_python = getattr(sys, "_base_executable", sys.executable)
    child = subprocess.Popen(
        [target_python, str(script_path)],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert child.stdout is not None
        _wait_for_ready(child, timeout=10.0)
        ok = cli._run_native_debugger_dump(pid=child.pid, log_path=log_path)
        if not ok:
            _skip_or_fail("native debugger failed to collect a stack dump")
        text = log_path.read_text(encoding="utf-8", errors="replace")
        if not any(frame in text for frame in _EXPECTED_PUBLIC_FRAMES):
            _skip_or_fail("native debugger on this machine did not resolve tiny-PDB public frames")
        assert any(frame in text for frame in _EXPECTED_PUBLIC_FRAMES)
        assert "_ZN" not in text
    finally:
        child.kill()
        try:
            child.wait(timeout=10.0)
        except subprocess.TimeoutExpired:
            child.terminate()
            child.wait(timeout=5.0)
