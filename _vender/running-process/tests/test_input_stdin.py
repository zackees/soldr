from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

import pytest

from running_process import RunningProcess

live = pytest.mark.live
skip_unless_github_actions = pytest.mark.skipif(
    os.environ.get("GITHUB_ACTIONS", "").lower() != "true",
    reason="requires GitHub Actions runner",
)
pytestmark = [live, skip_unless_github_actions]


def test_expect_matches_string_and_writes_to_stdin() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "print('prompt>'); sys.stdout.flush(); "
                "line = sys.stdin.readline().strip(); "
                "print('echo:' + line)"
            ),
        ],
        stdin=subprocess.PIPE,
    )
    match = process.expect("prompt>", timeout=5, action="typed text\n")
    assert match.matched == "prompt>"
    process.expect("echo:typed text", timeout=5)
    assert process.wait() == 0


def test_expect_matches_regex_groups() -> None:
    process = RunningProcess([sys.executable, "-c", "print('value=42')"])
    match = process.expect(re.compile(r"value=(\d+)"), timeout=5)
    assert match.groups == ("42",)
    process.wait()


def test_expect_rejects_invalid_stream_name() -> None:
    process = RunningProcess([sys.executable, "-c", "print('value=42')"])
    with pytest.raises(ValueError, match="stream must be"):
        process.expect("value", stream="bad")
    process.wait()


def test_run_with_text_input_capture_output() -> None:
    script = (
        "import sys; "
        "data = sys.stdin.read(); "
        "print(data.upper()); "
        "print('err:' + data.lower(), file=sys.stderr)"
    )
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            script,
        ],
        input="Hello\n",
        capture_output=True,
        text=True,
        check=True,
    )
    assert result.stdout is not None
    lines = result.stdout.strip().splitlines()
    assert "HELLO" in lines
    assert "err:hello" in lines
    assert result.stderr is None


def test_run_with_bytes_input_capture_output() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            "import sys; data = sys.stdin.buffer.read(); sys.stdout.buffer.write(data[::-1])",
        ],
        input=b"abc",
        capture_output=True,
        text=False,
    )
    assert result.stdout == b"cba"
    assert result.stderr is None


def test_run_input_and_stdin_conflict_raises() -> None:
    with pytest.raises(
        ValueError, match="stdin and input arguments may not both be used"
    ):
        RunningProcess.run(
            [sys.executable, "-c", "print('nope')"],
            input="hello",
            stdin=subprocess.PIPE,
        )


def test_write_requires_piped_stdin() -> None:
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    with pytest.raises(RuntimeError, match="stdin is not available"):
        process.write("ignored")
    process.wait()


def test_run_filter_input_uses_native_path_for_text() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "data = sys.stdin.read(); "
                "print(data.upper()); "
                "print('stderr:' + data.lower(), file=sys.stderr)"
            ),
        ],
        input="AbC\n",
        capture_output=True,
        text=True,
        timeout=5,
    )
    assert result.stdout is not None
    lines = result.stdout.splitlines()
    assert "ABC" in lines
    assert "stderr:abc" in lines
    assert result.stderr is None


def test_run_filter_input_uses_native_path_for_bytes() -> None:
    result = RunningProcess.run(
        [
            sys.executable,
            "-c",
            (
                "import sys; "
                "data = sys.stdin.buffer.read(); "
                "sys.stdout.buffer.write(data[::-1]); "
                "sys.stderr.buffer.write(data.upper())"
            ),
        ],
        input=b"abc",
        capture_output=True,
        text=False,
        timeout=5,
    )
    assert result.stdout is not None
    assert b"cba" in result.stdout
    assert b"ABC" in result.stdout
    assert result.stderr is None


def test_exec_script_runs_uv_shebang_with_lf() -> None:
    with tempfile.TemporaryDirectory() as temp_dir:
        script_path = Path(temp_dir) / "uv_script.py"
        script_path.write_text(
            "#!/usr/bin/env -S uv run --script\n" "print('uv shebang works')\n",
            encoding="utf-8",
        )

        result = RunningProcess.exec_script(script_path)
        assert result.stdout is not None
        assert result.stdout.strip() == "uv shebang works"


def test_exec_script_runs_uv_shebang_with_crlf() -> None:
    with tempfile.TemporaryDirectory() as temp_dir:
        script_path = Path(temp_dir) / "uv_script_crlf.py"
        script_path.write_bytes(
            b"#!/usr/bin/env -S uv run --script\r\n"
            b"print('uv shebang crlf works')\r\n"
        )

        result = RunningProcess.exec_script(script_path)
        assert result.stdout is not None
        assert result.stdout.strip() == "uv shebang crlf works"


def test_exec_script_without_shebang_raises() -> None:
    with tempfile.TemporaryDirectory() as temp_dir:
        script_path = Path(temp_dir) / "plain.py"
        script_path.write_text("print('no shebang')\n", encoding="utf-8")

        with pytest.raises(ValueError, match="does not start with a shebang"):
            RunningProcess.exec_script(script_path)
