"""Tests for the CI log-timestamping trio.

`cross-compile-all-targets.yml` installs `run_with_ts.sh` as its
`defaults.run.shell`, so **every** `run:` step in that workflow has its exit
code decided by that wrapper rather than by bash directly. A regression there
would report failing steps as green -- the whole workflow would go quietly
useless while looking healthy.

The wrapper is subtle enough to be worth pinning: it does not rely on
`pipefail` (which yields the *rightmost* non-zero code, i.e. the
timestamper's) but walks `PIPESTATUS` and surfaces the *first* non-zero, so
the step's real failure reason survives. Nothing tested any of this.

Also pinned here: the two timestampers claim in their docstrings to produce
byte-identical output formats, and nothing enforced that either.
"""

from __future__ import annotations

import re
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

SCRIPTS = Path(__file__).resolve().parents[1] / ".github" / "scripts"
TS_STEP = SCRIPTS / "ts_step.py"
RUN_WITH_TS_PY = SCRIPTS / "run_with_ts.py"
RUN_WITH_TS_SH = SCRIPTS / "run_with_ts.sh"

# `{seconds:7.2f} ` -- right-aligned in 7 columns plus one trailing space.
PREFIX_RE = re.compile(rb"^ *\d+\.\d{2} ")


def _ts_step(stdin: bytes) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-u", str(TS_STEP)],
        input=stdin,
        capture_output=True,
    )


# --- ts_step.py: the pure stdin -> stdout transformer ---------------------


def test_every_line_gets_an_elapsed_prefix():
    result = _ts_step(b"alpha\nbeta\ngamma\n")
    lines = result.stdout.splitlines()
    assert len(lines) == 3
    for line, expected in zip(lines, (b"alpha", b"beta", b"gamma")):
        match = PREFIX_RE.match(line)
        assert match, f"no elapsed prefix on {line!r}"
        assert line[match.end() :] == expected
    assert result.returncode == 0


def test_line_body_is_passed_through_byte_for_byte():
    # ANSI colour and non-UTF-8 bytes must survive untouched -- the whole
    # point of working on the raw buffers. A str-mode rewrite would mangle
    # both, and coloured cargo output is the visible symptom.
    body = b"\x1b[32mok\x1b[0m \xff\xfe raw"
    result = _ts_step(body + b"\n")
    line = result.stdout.splitlines()[0]
    assert line[PREFIX_RE.match(line).end() :] == body


def test_a_final_line_without_a_newline_is_still_emitted():
    # Tools that die mid-write leave an unterminated last line. Dropping it
    # would hide the most interesting line in the log.
    result = _ts_step(b"first\nno trailing newline")
    lines = result.stdout.splitlines()
    assert len(lines) == 2
    assert lines[1][PREFIX_RE.match(lines[1]).end() :] == b"no trailing newline"


def test_crlf_input_keeps_its_carriage_return():
    result = _ts_step(b"windows\r\n")
    assert result.stdout.endswith(b"windows\r\n")


def test_empty_input_is_not_an_error():
    result = _ts_step(b"")
    assert result.returncode == 0
    assert result.stdout == b""


def test_elapsed_seconds_are_monotonically_non_decreasing():
    result = _ts_step(b"a\nb\nc\nd\n")
    values = [
        float(PREFIX_RE.match(line).group().strip())
        for line in result.stdout.splitlines()
    ]
    assert values == sorted(values), values


# --- run_with_ts.py: the wrapper that executes a command ------------------


def _run_with_ts(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-u", str(RUN_WITH_TS_PY), *args],
        capture_output=True,
    )


def test_wrapper_propagates_the_commands_exit_code():
    # The reason this script exists: in PowerShell, `cmd | python ts_step.py`
    # reports python's exit code, not cmd's. If this regressed, failing
    # steps would report success.
    result = _run_with_ts(sys.executable, "-c", "raise SystemExit(7)")
    assert result.returncode == 7


def test_wrapper_returns_zero_for_a_successful_command():
    result = _run_with_ts(sys.executable, "-c", "print('fine')")
    assert result.returncode == 0
    assert b"fine" in result.stdout


def test_wrapper_merges_stderr_into_the_timestamped_stream():
    result = _run_with_ts(
        sys.executable,
        "-c",
        "import sys; sys.stderr.write('to-stderr\\n'); sys.stderr.flush()",
    )
    line = result.stdout.splitlines()[0]
    assert PREFIX_RE.match(line), f"stderr line was not timestamped: {line!r}"
    assert line.endswith(b"to-stderr")


def test_missing_command_reports_127_not_a_traceback():
    result = _run_with_ts("definitely-not-a-real-binary-xyz")
    assert result.returncode == 127
    assert b"Traceback" not in result.stderr


def test_no_arguments_is_a_usage_error():
    result = _run_with_ts()
    assert result.returncode == 2
    assert b"usage:" in result.stderr


def test_both_timestampers_emit_the_same_prefix_shape():
    # Both docstrings promise the formats match "byte-for-byte". They are
    # separate implementations of the same f-string, so nothing but this
    # keeps them aligned.
    piped = _ts_step(b"same\n").stdout.splitlines()[0]
    wrapped = _run_with_ts(sys.executable, "-c", "print('same')").stdout.splitlines()[0]
    assert PREFIX_RE.match(piped).end() == PREFIX_RE.match(wrapped).end()
    assert piped.endswith(b"same") and wrapped.endswith(b"same")


# --- run_with_ts.sh: the GHA `defaults.run.shell` wrapper -----------------


def _bash_path(path: Path) -> str:
    """`C:\\x\\y` -> `/c/x/y`, which Git Bash understands and a native
    POSIX bash never sees."""
    text = str(path).replace("\\", "/")
    if len(text) > 2 and text[1] == ":" and text[2] == "/":
        return f"/{text[0].lower()}/{text[3:]}"
    return text


def _find_working_bash() -> str | None:
    """Return a bash that can actually run the wrapper, or None.

    A name lookup is not enough: on Windows `shutil.which("bash")` finds
    WSL's `bash.exe` first, which cannot execute a Windows-side script at
    all ("execvpe(/bin/bash) failed"). Probing by running the real wrapper
    means the interpreter AND the path translation are both verified before
    any test trusts them -- and that a skip is a genuine "cannot run here"
    rather than a silently wrong pass.
    """
    candidates = [c for c in (shutil.which("bash"),) if c]
    candidates += [
        p
        for p in (
            "C:/Program Files/Git/bin/bash.exe",
            "C:/Program Files/Git/usr/bin/bash.exe",
        )
        if Path(p).is_file()
    ]
    import tempfile

    for candidate in candidates:
        with tempfile.TemporaryDirectory() as td:
            step = Path(td) / "probe.sh"
            step.write_text("exit 0\n", encoding="utf-8", newline="\n")
            try:
                probe = subprocess.run(
                    [candidate, _bash_path(RUN_WITH_TS_SH), _bash_path(step)],
                    capture_output=True,
                    timeout=60,
                )
            except (OSError, subprocess.TimeoutExpired):
                continue
            if probe.returncode == 0:
                return candidate
    return None


BASH = _find_working_bash()

bash_available = pytest.mark.skipif(
    BASH is None,
    reason="no bash that can run run_with_ts.sh (needs bash + python3; "
    "WSL bash cannot execute a Windows-side script)",
)


def _run_shell_wrapper(tmp_path: Path, body: str) -> subprocess.CompletedProcess:
    """Drive the wrapper exactly as GitHub Actions does: the step body is
    written to a temp file whose path is passed as `{0}`."""
    step = tmp_path / "step.sh"
    step.write_text(body, encoding="utf-8", newline="\n")
    assert BASH is not None
    return subprocess.run(
        [BASH, _bash_path(RUN_WITH_TS_SH), _bash_path(step)],
        capture_output=True,
        cwd=SCRIPTS.parents[1],
    )


@bash_available
def test_a_failing_step_body_does_not_report_success(tmp_path):
    # THE test. If this ever returns 0, every failing step in
    # cross-compile-all-targets.yml goes green.
    result = _run_shell_wrapper(tmp_path, "echo working\nexit 3\n")
    assert result.returncode == 3, result.stdout + result.stderr


@bash_available
def test_a_successful_step_body_reports_success(tmp_path):
    result = _run_shell_wrapper(tmp_path, "echo working\n")
    assert result.returncode == 0, result.stdout + result.stderr


@bash_available
def test_the_step_bodys_own_pipefail_failure_survives(tmp_path):
    # The inner bash runs with `-eo pipefail`, so a failure mid-pipeline
    # inside the step body must still reach the caller rather than being
    # swallowed by the trailing `true`.
    result = _run_shell_wrapper(tmp_path, "false | cat\necho unreachable\n")
    assert result.returncode != 0


@bash_available
def test_the_wrapper_timestamps_the_step_output(tmp_path):
    result = _run_shell_wrapper(tmp_path, "echo hello-from-step\n")
    line = result.stdout.splitlines()[0]
    assert PREFIX_RE.match(line), f"step output was not timestamped: {line!r}"
    assert line.endswith(b"hello-from-step")


@bash_available
def test_step_stderr_is_captured_and_timestamped(tmp_path):
    result = _run_shell_wrapper(tmp_path, "echo oops >&2\n")
    line = result.stdout.splitlines()[0]
    assert PREFIX_RE.match(line)
    assert line.endswith(b"oops")
