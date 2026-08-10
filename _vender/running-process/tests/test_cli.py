from __future__ import annotations

import io
from pathlib import Path
from types import SimpleNamespace

from running_process import cli


class _FakeProcess:
    def __init__(
        self,
        *,
        pid: int = 4321,
        returncode: int = 0,
        stdout_bytes: bytes = b"",
        stderr_bytes: bytes = b"",
    ) -> None:
        self.pid = pid
        self.returncode = returncode
        self.kill_called = False
        self.wait_calls = 0
        self.poll_calls = 0
        self.stdout = io.BytesIO(stdout_bytes)
        self.stderr = io.BytesIO(stderr_bytes)

    def poll(self) -> int | None:
        self.poll_calls += 1
        return self.returncode

    def wait(self, timeout: float | None = None) -> int:
        del timeout
        self.wait_calls += 1
        return self.returncode

    def kill(self) -> None:
        self.kill_called = True


class _PollingProcess:
    def __init__(self, polls_before_exit: int) -> None:
        self.pid = 99
        self.returncode = 0
        self._polls_before_exit = polls_before_exit
        self.stdout = io.BytesIO(b"tick\n")
        self.stderr = io.BytesIO()

    def poll(self) -> int | None:
        if self._polls_before_exit > 0:
            self._polls_before_exit -= 1
            return None
        return self.returncode

    def wait(self, timeout: float | None = None) -> int:
        del timeout
        return self.returncode


class _BufferedTextStream:
    def __init__(self) -> None:
        self.buffer = io.BytesIO()

    def flush(self) -> None:
        return None


class _Read1OnlyStream:
    def __init__(self, chunks: list[bytes]) -> None:
        self._chunks = list(chunks)
        self.closed = False

    def read1(self, size: int) -> bytes:
        del size
        if not self._chunks:
            return b""
        return self._chunks.pop(0)

    def read(self, size: int) -> bytes:
        del size
        raise AssertionError("read() should not be used when read1() is available")

    def close(self) -> None:
        self.closed = True


def _seed_child_output_diagnostics(
    child: object,
    *,
    stdout_bytes: bytes = b"",
    stderr_bytes: bytes = b"",
    timed_out: bool,
    returncode: int | None,
    idle_for_seconds: float = 0.0,
) -> cli._ChildOutputDiagnostics:
    diagnostics = cli._attach_child_output_diagnostics(child)
    if stdout_bytes:
        diagnostics.stdout.record(stdout_bytes)
    if stderr_bytes:
        diagnostics.stderr.record(stderr_bytes)
    diagnostics.stdout.closed = True
    diagnostics.stderr.closed = True
    cli._finalize_child_output_diagnostics(
        diagnostics,
        idle_for_seconds=idle_for_seconds,
        timed_out=timed_out,
        returncode=returncode,
    )
    return diagnostics


def test_normalize_command_strips_separator() -> None:
    assert cli._normalize_command(["--", "python", "-m", "ci.test"]) == [
        "python",
        "-m",
        "ci.test",
    ]


def test_parse_args_accepts_find_leaks_flag() -> None:
    args = cli._parse_args(["--find-leaks", "--", "python", "-m", "ci.test"])

    assert args.find_leaks is True
    assert args.command == ["--", "python", "-m", "ci.test"]


def test_bounded_tail_buffer_truncates_old_bytes() -> None:
    buffer = cli._BoundedTailBuffer(5)

    buffer.append(b"abc")
    buffer.append(b"def")

    assert buffer.truncated is True
    assert buffer.decode() == "bcdef"


def test_child_stream_diagnostics_records_chunk_metadata() -> None:
    diagnostics = cli._ChildStreamDiagnostics()

    diagnostics.record(b"alpha")
    diagnostics.record(b"beta")
    diagnostics.closed = True

    metadata = diagnostics.as_metadata()

    assert metadata["total_bytes"] == 9
    assert metadata["chunk_count"] == 2
    assert metadata["closed"] is True
    assert metadata["last_chunk_utc"] is not None
    assert metadata["tail_truncated"] is False
    assert metadata["tail_text"] == "alphabeta"


def test_build_child_output_extra_metadata_wraps_diagnostics() -> None:
    child = _FakeProcess()

    _seed_child_output_diagnostics(
        child,
        stdout_bytes=b"stdout\n",
        stderr_bytes=b"stderr\n",
        timed_out=False,
        returncode=3,
        idle_for_seconds=0.25,
    )

    extra_metadata = cli._build_child_output_extra_metadata(child)

    assert extra_metadata is not None
    assert extra_metadata["child_output"]["returncode"] == 3
    assert extra_metadata["child_output"]["stdout"]["tail_text"] == "stdout\n"
    assert extra_metadata["child_output"]["stderr"]["tail_text"] == "stderr\n"


def test_build_child_output_extra_metadata_returns_none_without_diagnostics() -> None:
    assert cli._build_child_output_extra_metadata(_FakeProcess()) is None


def test_build_diagnostic_dump_kwargs_omits_empty_extra_metadata(tmp_path: Path) -> None:
    dump_kwargs = cli._build_diagnostic_dump_kwargs(
        reason="timeout",
        command=["python", "-m", "ci.test"],
        pid=77,
        returncode=None,
        timeout_seconds=1.5,
        dump_dir=tmp_path,
        extra_metadata=None,
    )

    assert dump_kwargs == {
        "reason": "timeout",
        "command": ["python", "-m", "ci.test"],
        "pid": 77,
        "returncode": None,
        "timeout_seconds": 1.5,
        "dump_dir": tmp_path,
    }


def test_finalize_child_output_diagnostics_clamps_negative_idle_time() -> None:
    diagnostics = cli._ChildOutputDiagnostics()

    cli._finalize_child_output_diagnostics(
        diagnostics,
        idle_for_seconds=-1.0,
        timed_out=False,
        returncode=0,
    )

    assert diagnostics.idle_for_seconds == 0.0
    assert diagnostics.timed_out is False
    assert diagnostics.returncode == 0


def test_cli_passes_in_running_process_to_child(monkeypatch) -> None:
    seen: dict[str, object] = {}
    fake_process = _FakeProcess(returncode=0)

    def fake_popen(command, env, stdout, stderr):
        seen["command"] = list(command)
        seen["env"] = env
        seen["stdout"] = stdout
        seen["stderr"] = stderr
        return fake_process

    monkeypatch.setattr(cli.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(
        cli,
        "_wait_for_child_with_activity_timeout",
        lambda child, timeout: (0, False),
    )

    assert cli.main(["--", "python", "-m", "ci.test"]) == 0
    assert seen["command"] == ["python", "-m", "ci.test"]
    assert isinstance(seen["env"], dict)
    assert seen["env"][cli.IN_RUNNING_PROCESS_ENV] == cli.IN_RUNNING_PROCESS_VALUE, (
        "pytest must be run through the running-process CLI so it provides "
        "IN_RUNNING_PROCESS=running-process-cli and can auto-dump stacks on hangs; "
        "do not manually set IN_RUNNING_PROCESS in agent-driven test commands."
    )
    assert seen["stdout"] is cli.subprocess.PIPE
    assert seen["stderr"] is cli.subprocess.PIPE


def test_find_leaks_sets_originator_env_and_reports_survivors(monkeypatch) -> None:
    seen: dict[str, object] = {}
    stderr = io.StringIO()
    fake_process = _FakeProcess(returncode=0)
    fake_leak = SimpleNamespace(
        pid=9001,
        name="python",
        command="python leaked_worker.py",
        originator="ignored",
        parent_pid=1234,
        parent_alive=False,
    )

    def fake_popen(command, env, stdout, stderr):
        seen["command"] = list(command)
        seen["env"] = env
        seen["stdout"] = stdout
        seen["stderr"] = stderr
        return fake_process

    monkeypatch.setattr(cli.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(
        cli,
        "_wait_for_child_with_activity_timeout",
        lambda child, timeout: (0, False),
    )
    monkeypatch.setattr(cli.os, "getpid", lambda: 1234)
    monkeypatch.setattr(cli, "_leak_originator_tool", lambda: "RUNNING_PROCESS_LEAK_TEST")
    monkeypatch.setattr(cli, "find_processes_by_originator", lambda tool: [fake_leak])
    monkeypatch.setattr(cli.sys, "stderr", stderr)

    code = cli.run_command(["python", "-m", "ci.test"], find_leaks=True)

    assert code == 0
    assert seen["command"] == ["python", "-m", "ci.test"]
    assert seen["env"][cli.ORIGINATOR_ENV_VAR] == "RUNNING_PROCESS_LEAK_TEST:1234"
    rendered = stderr.getvalue()
    assert "detected 1 leaked descendant process(es)" in rendered
    assert "pid=9001" in rendered
    assert "python leaked_worker.py" in rendered


def test_find_process_leaks_returns_sorted_results(monkeypatch) -> None:
    monkeypatch.setattr(
        cli,
        "find_processes_by_originator",
        lambda tool: [
            SimpleNamespace(pid=7),
            SimpleNamespace(pid=3),
            SimpleNamespace(pid=5),
        ],
    )

    leaks = cli._find_process_leaks("RUNNING_PROCESS_LEAK_TEST")

    assert [leak.pid for leak in leaks] == [3, 5, 7]


def test_timeout_collects_diagnostics_and_kills_child(monkeypatch, tmp_path: Path) -> None:
    fake_process = _FakeProcess()
    seen: dict[str, object] = {}

    def fake_popen(command, env, stdout, stderr):
        seen["command"] = list(command)
        seen["env"] = env
        seen["stdout"] = stdout
        seen["stderr"] = stderr
        return fake_process

    def fake_dump(**kwargs):
        seen["dump"] = kwargs
        return tmp_path / "timeout.json"

    def fake_kill(child):
        seen["killed_child"] = child
        child.kill()

    monkeypatch.setattr(cli.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(
        cli,
        "_wait_for_child_with_activity_timeout",
        lambda child, timeout: (None, True),
    )
    monkeypatch.setattr(cli, "_dump_diagnostics", fake_dump)
    monkeypatch.setattr(cli, "_kill_supervised_process", fake_kill)

    code = cli.run_command(
        ["python", "-m", "ci.test"],
        timeout=1.5,
        stack_dump_dir=tmp_path,
    )

    assert code == cli.DEFAULT_STACK_DUMP_TIMEOUT_EXIT_CODE
    assert fake_process.kill_called is True
    assert seen["killed_child"] is fake_process
    assert seen["dump"] == {
        "reason": "timeout",
        "command": ["python", "-m", "ci.test"],
        "pid": 4321,
        "returncode": None,
        "timeout_seconds": 1.5,
        "dump_dir": tmp_path,
    }


def test_timeout_attaches_child_output_metadata(monkeypatch, tmp_path: Path) -> None:
    fake_process = _FakeProcess()
    seen: dict[str, object] = {}

    def fake_popen(command, env, stdout, stderr):
        del command, env, stdout, stderr
        return fake_process

    def fake_wait(child, timeout):
        del timeout
        _seed_child_output_diagnostics(
            child,
            stdout_bytes=b"last stdout line\n",
            stderr_bytes=b"last stderr line\n",
            timed_out=True,
            returncode=None,
            idle_for_seconds=1.5,
        )
        return None, True

    def fake_dump(**kwargs):
        seen["dump"] = kwargs
        return tmp_path / "timeout.json"

    monkeypatch.setattr(cli.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(cli, "_wait_for_child_with_activity_timeout", fake_wait)
    monkeypatch.setattr(cli, "_dump_diagnostics", fake_dump)

    code = cli.run_command(
        ["python", "-m", "ci.test"],
        timeout=1.5,
        stack_dump_dir=tmp_path,
    )

    assert code == cli.DEFAULT_STACK_DUMP_TIMEOUT_EXIT_CODE
    child_output = seen["dump"]["extra_metadata"]["child_output"]
    assert child_output["timed_out"] is True
    assert child_output["returncode"] is None
    assert child_output["stdout"]["tail_text"] == "last stdout line\n"
    assert child_output["stderr"]["tail_text"] == "last stderr line\n"


def test_timeout_can_disable_auto_stack_dumping(monkeypatch, tmp_path: Path) -> None:
    fake_process = _FakeProcess()
    dump_called = False
    killed = False

    def fake_popen(command, env, stdout, stderr):
        del command, env, stdout, stderr
        return fake_process

    def fake_dump(**kwargs):
        del kwargs
        nonlocal dump_called
        dump_called = True
        return tmp_path / "timeout.json"

    def fake_kill(child):
        nonlocal killed
        killed = True
        child.kill()

    monkeypatch.setattr(cli.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(
        cli,
        "_wait_for_child_with_activity_timeout",
        lambda child, timeout: (None, True),
    )
    monkeypatch.setattr(cli, "_dump_diagnostics", fake_dump)
    monkeypatch.setattr(cli, "_kill_supervised_process", fake_kill)

    code = cli.run_command(
        ["python", "-m", "ci.test"],
        timeout=1.5,
        auto_stack_dumping=False,
        stack_dump_dir=tmp_path,
    )

    assert code == cli.DEFAULT_STACK_DUMP_TIMEOUT_EXIT_CODE
    assert dump_called is False
    assert killed is True


def test_abnormal_exit_collects_diagnostics(monkeypatch, tmp_path: Path) -> None:
    fake_process = _FakeProcess(returncode=3)
    seen: dict[str, object] = {}

    def fake_popen(command, env, stdout, stderr):
        seen["command"] = list(command)
        seen["env"] = env
        seen["stdout"] = stdout
        seen["stderr"] = stderr
        return fake_process

    def fake_dump(**kwargs):
        seen["dump"] = kwargs
        return tmp_path / "abnormal.json"

    monkeypatch.setattr(cli.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(
        cli,
        "_wait_for_child_with_activity_timeout",
        lambda child, timeout: (3, False),
    )
    monkeypatch.setattr(cli, "_dump_diagnostics", fake_dump)

    code = cli.run_command(
        ["python", "-m", "ci.test"],
        stack_dump_dir=tmp_path,
    )

    assert code == 3
    assert seen["dump"] == {
        "reason": "abnormal-exit",
        "command": ["python", "-m", "ci.test"],
        "pid": 4321,
        "returncode": 3,
        "timeout_seconds": None,
        "dump_dir": tmp_path,
    }


def test_abnormal_exit_attaches_child_output_metadata(monkeypatch, tmp_path: Path) -> None:
    fake_process = _FakeProcess(returncode=3)
    seen: dict[str, object] = {}

    def fake_popen(command, env, stdout, stderr):
        seen["command"] = list(command)
        seen["env"] = env
        seen["stdout"] = stdout
        seen["stderr"] = stderr
        return fake_process

    def fake_wait(child, timeout):
        del timeout
        _seed_child_output_diagnostics(
            child,
            stdout_bytes=b"last stdout line\n",
            stderr_bytes=b"last stderr line\n",
            timed_out=False,
            returncode=3,
            idle_for_seconds=0.25,
        )
        return 3, False

    def fake_dump(**kwargs):
        seen["dump"] = kwargs
        return tmp_path / "abnormal.json"

    monkeypatch.setattr(cli.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(cli, "_wait_for_child_with_activity_timeout", fake_wait)
    monkeypatch.setattr(cli, "_dump_diagnostics", fake_dump)

    code = cli.run_command(
        ["python", "-m", "ci.test"],
        stack_dump_dir=tmp_path,
    )

    assert code == 3
    child_output = seen["dump"]["extra_metadata"]["child_output"]
    assert child_output["returncode"] == 3
    assert child_output["timed_out"] is False
    assert child_output["idle_for_seconds"] is not None
    assert child_output["tail_limit_bytes"] > 0
    assert child_output["stdout"] == {
        "total_bytes": 17,
        "chunk_count": 1,
        "closed": True,
        "last_chunk_utc": child_output["stdout"]["last_chunk_utc"],
        "tail_truncated": False,
        "tail_text": "last stdout line\n",
    }
    assert child_output["stderr"] == {
        "total_bytes": 17,
        "chunk_count": 1,
        "closed": True,
        "last_chunk_utc": child_output["stderr"]["last_chunk_utc"],
        "tail_truncated": False,
        "tail_text": "last stderr line\n",
    }


def test_wait_for_child_timeout_is_based_on_output_activity(monkeypatch) -> None:
    process = _PollingProcess(polls_before_exit=20)
    monotonic_values = iter([0.0, 0.0, 0.02, 0.04, 0.06, 0.08, 0.11, 0.13])
    monkeypatch.setattr(cli.time, "monotonic", lambda: next(monotonic_values))
    monkeypatch.setattr(cli.time, "sleep", lambda seconds: None)
    stdout = _BufferedTextStream()
    stderr = _BufferedTextStream()
    monkeypatch.setattr(cli.sys, "stdout", stdout)
    monkeypatch.setattr(cli.sys, "stderr", stderr)

    returncode, timed_out = cli._wait_for_child_with_activity_timeout(process, timeout=0.1)

    assert timed_out is True
    assert returncode is None
    assert stdout.buffer.getvalue() == b"tick\n"
    assert stderr.buffer.getvalue() == b""


def test_wait_for_child_returns_after_exit_and_stream_drain(monkeypatch) -> None:
    process = _PollingProcess(polls_before_exit=1)
    call_count = [0]

    def fake_monotonic():
        val = call_count[0] * 0.02
        call_count[0] += 1
        return val

    monkeypatch.setattr(cli.time, "monotonic", fake_monotonic)
    monkeypatch.setattr(cli.time, "sleep", lambda seconds: None)
    stdout = _BufferedTextStream()
    stderr = _BufferedTextStream()
    monkeypatch.setattr(cli.sys, "stdout", stdout)
    monkeypatch.setattr(cli.sys, "stderr", stderr)

    returncode, timed_out = cli._wait_for_child_with_activity_timeout(process, timeout=0.5)

    assert timed_out is False
    assert returncode == 0
    assert stdout.buffer.getvalue() == b"tick\n"


def test_wait_for_child_records_recent_output_tail(monkeypatch) -> None:
    process = _PollingProcess(polls_before_exit=1)
    call_count = [0]

    def fake_monotonic():
        val = call_count[0] * 0.02
        call_count[0] += 1
        return val

    monkeypatch.setattr(cli.time, "monotonic", fake_monotonic)
    monkeypatch.setattr(cli.time, "sleep", lambda seconds: None)
    monkeypatch.setattr(cli.sys, "stdout", _BufferedTextStream())
    monkeypatch.setattr(cli.sys, "stderr", _BufferedTextStream())

    returncode, timed_out = cli._wait_for_child_with_activity_timeout(process, timeout=0.5)

    metadata = cli._child_output_metadata(process)

    assert timed_out is False
    assert returncode == 0
    assert metadata is not None
    assert metadata["returncode"] == 0
    assert metadata["timed_out"] is False
    assert metadata["stdout"]["total_bytes"] == len(b"tick\n")
    assert metadata["stdout"]["tail_text"] == "tick\n"
    assert metadata["stderr"]["tail_text"] == ""
    assert metadata["idle_for_seconds"] is not None


def test_stream_reader_prefers_read1_for_pipe_like_streams() -> None:
    source = _Read1OnlyStream([b"alpha", b"beta"])
    sink = _BufferedTextStream()
    touched = 0

    def touch() -> None:
        nonlocal touched
        touched += 1

    cli._stream_reader(source, sink, touch_activity=touch)

    assert sink.buffer.getvalue() == b"alphabeta"
    assert touched == 2
    assert source.closed is True


def test_dump_diagnostics_writes_metadata_and_py_spy_log(monkeypatch, tmp_path: Path) -> None:
    py_spy_log = tmp_path / "py-spy.log"
    native_log = tmp_path / "native.log"

    def fake_py_spy_dump(*, pid: int | None, log_path: Path) -> bool:
        assert pid == 77
        py_spy_log.write_text("py-spy output", encoding="utf-8")
        log_path.write_text("py-spy output", encoding="utf-8")
        return True

    def fake_native_dump(*, pid: int | None, log_path: Path) -> bool:
        assert pid == 77
        native_log.write_text("native output", encoding="utf-8")
        log_path.write_text("native output", encoding="utf-8")
        return True

    monkeypatch.setattr(cli, "_run_py_spy_dump", fake_py_spy_dump)
    monkeypatch.setattr(cli, "_run_native_debugger_dump", fake_native_dump)

    metadata_path = cli._dump_diagnostics(
        reason="timeout",
        command=["python", "-m", "ci.test"],
        pid=77,
        returncode=None,
        timeout_seconds=5.0,
        dump_dir=tmp_path,
        extra_metadata={
            "child_output": {
                "stdout": {
                    "bytes_seen": 4,
                    "tail": "tail",
                    "truncated": False,
                }
            }
        },
    )

    assert metadata_path.is_file()
    assert metadata_path.suffix == ".json"
    metadata = metadata_path.read_text(encoding="utf-8")
    assert '"reason": "timeout"' in metadata
    assert '"pid": 77' in metadata
    assert '"child_output"' in metadata
    assert any(path.name.endswith(".py-spy.log") for path in tmp_path.iterdir())
    assert any(path.name.endswith(".native-debugger.log") for path in tmp_path.iterdir())


def test_run_py_spy_dump_records_unavailable_tool(monkeypatch, tmp_path: Path) -> None:
    log_path = tmp_path / "dump.log"
    monkeypatch.setattr(cli.shutil, "which", lambda name: None)
    result = cli._run_py_spy_dump(pid=123, log_path=log_path)

    assert result is False
    assert "py-spy unavailable" in log_path.read_text(encoding="utf-8")


def test_native_debugger_command_prefers_lldb(monkeypatch) -> None:
    monkeypatch.setattr(
        cli.shutil,
        "which",
        lambda name: "C:/tools/lldb.exe" if name == "lldb" else "C:/tools/gdb.exe",
    )

    commands = cli._native_debugger_commands(123)

    assert commands
    assert commands[0][0] == "C:/tools/lldb.exe"
    assert "thread backtrace all" in commands[0]
    assert commands[1][0] == "C:/tools/gdb.exe"


def test_demangle_native_debugger_text_uses_cxxfilt(monkeypatch) -> None:
    seen: list[list[str]] = []

    def fake_run(command, **kwargs):
        seen.append(command)

        class Result:
            returncode = 0
            stdout = "running_process::NativeProcess::wait::h6b7a0fd0be7a0f11\n"

        return Result()

    monkeypatch.setattr(cli.shutil, "which", lambda name: "C:/tools/c++filt.exe")
    monkeypatch.setattr(cli.subprocess, "run", fake_run)

    rendered = cli._demangle_native_debugger_text(
        "frame _ZN20running_process13NativeProcess4wait17h6b7a0fd0be7a0f11E"
    )

    assert rendered == "frame running_process::NativeProcess::wait"
    assert seen == [["C:/tools/c++filt.exe"]]


def test_run_native_debugger_dump_falls_back_after_failed_debugger(
    monkeypatch, tmp_path: Path
) -> None:
    log_path = tmp_path / "native.log"
    monkeypatch.setattr(
        cli,
        "_native_debugger_commands",
        lambda pid: [["lldb", "--batch", str(pid)], ["gdb", "--batch", str(pid)]],
    )

    def fake_run(command, **kwargs):
        del kwargs

        class Result:
            def __init__(self, returncode: int, stdout: str) -> None:
                self.returncode = returncode
                self.stdout = stdout
                self.stderr = ""

        if command[0] == "lldb":
            return Result(1, "")
        return Result(
            0,
            "_ZN20running_process13NativeProcess4wait17h6b7a0fd0be7a0f11E\n",
        )

    monkeypatch.setattr(
        cli.shutil,
        "which",
        lambda name: "C:/tools/c++filt.exe" if name == "c++filt" else None,
    )

    def fake_cxxfilt(command, **kwargs):
        del kwargs
        if command == ["C:/tools/c++filt.exe"]:
            class Result:
                returncode = 0
                stdout = "running_process::NativeProcess::wait::h6b7a0fd0be7a0f11\n"
                stderr = ""

            return Result()
        return fake_run(command)

    monkeypatch.setattr(cli.subprocess, "run", fake_cxxfilt)

    assert cli._run_native_debugger_dump(pid=123, log_path=log_path) is True
    text = log_path.read_text(encoding="utf-8")
    assert "$ gdb --batch 123" in text
    assert "running_process::NativeProcess::wait" in text
    assert "_ZN20running_process13NativeProcess4wait17h6b7a0fd0be7a0f11E" not in text


def test_run_native_debugger_dump_records_unavailable_tool(monkeypatch, tmp_path: Path) -> None:
    log_path = tmp_path / "native.log"
    monkeypatch.setattr(cli, "_native_debugger_commands", lambda pid: [])

    result = cli._run_native_debugger_dump(pid=123, log_path=log_path)

    assert result is False
    assert "native debugger unavailable" in log_path.read_text(encoding="utf-8")
