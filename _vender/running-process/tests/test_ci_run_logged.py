from __future__ import annotations

import importlib
import io
import json
from pathlib import Path


def _load_run_logged_module():
    return importlib.import_module("ci.run_logged")


class _Cp1252Stdout:
    encoding = "cp1252"

    def __init__(self) -> None:
        self.buffer = io.BytesIO()

    def write(self, text: str) -> int:
        text.encode(self.encoding)
        return len(text)

    def flush(self) -> None:
        return None


def test_write_console_line_replaces_unencodable_characters(monkeypatch) -> None:
    module = _load_run_logged_module()
    fake_stdout = _Cp1252Stdout()
    monkeypatch.setattr(module.sys, "stdout", fake_stdout)

    module._write_console_line("📦 hello\n")

    assert fake_stdout.buffer.getvalue().decode("cp1252") == "? hello\n"


def test_stack_dump_timeout_defaults_and_clamps(monkeypatch) -> None:
    module = _load_run_logged_module()

    monkeypatch.delenv(module._STACK_DUMP_TIMEOUT_ENV, raising=False)
    assert module._stack_dump_timeout_seconds() == module._DEFAULT_STACK_DUMP_TIMEOUT_SECONDS

    monkeypatch.setenv(module._STACK_DUMP_TIMEOUT_ENV, "3")
    assert module._stack_dump_timeout_seconds() == 5.0

    monkeypatch.setenv(module._STACK_DUMP_TIMEOUT_ENV, "bad")
    assert module._stack_dump_timeout_seconds() == module._DEFAULT_STACK_DUMP_TIMEOUT_SECONDS


def test_child_env_sets_python_faulthandler(monkeypatch) -> None:
    module = _load_run_logged_module()

    monkeypatch.delenv("PYTHONFAULTHANDLER", raising=False)

    env = module._child_env()

    assert env["PYTHONFAULTHANDLER"] == "1"


def test_run_analytics_tracks_last_pytest_nodeid() -> None:
    module = _load_run_logged_module()
    analytics = module.RunAnalytics(command=["pytest"], pid=123)

    analytics.record_line(
        "\x1b[32mtests/test_pty_support.py::test_target_case PASSED [ 50%]\x1b[0m\n"
    )

    assert analytics.last_test_nodeid == "tests/test_pty_support.py::test_target_case"
    assert (
        analytics.last_nonempty_line
        == "tests/test_pty_support.py::test_target_case PASSED [ 50%]"
    )


def test_write_analytics_persists_failure_tail(tmp_path: Path) -> None:
    module = _load_run_logged_module()
    analytics = module.RunAnalytics(command=["pytest", "-vv"], pid=456)
    analytics.record_line("tests/test_pty_support.py::test_target_case\n")
    analytics.record_line("spawn-path guard failed:\n")

    analytics_path = module._write_analytics(tmp_path / "test.log", analytics, returncode=1)

    payload = json.loads(analytics_path.read_text(encoding="utf-8"))
    assert payload["returncode"] == 1
    assert payload["last_test_nodeid"] == "tests/test_pty_support.py::test_target_case"
    assert payload["fault_lines"] == ["spawn-path guard failed:"]


def test_run_analytics_captures_pytest_failure_excerpt() -> None:
    module = _load_run_logged_module()
    analytics = module.RunAnalytics(command=["pytest", "-vv"], pid=456)

    for line in (
        "tests/test_cli.py::test_target_case FAILED [ 50%]\n",
        "=================================== FAILURES ===================================\n",
        "________________________ test_target_case ________________________\n",
        "E       assert left == right\n",
        "tests/test_cli.py:42: AssertionError\n",
        "later summary noise\n",
    ):
        analytics.record_line(line)

    assert list(analytics.pytest_failure_excerpt) == [
        "tests/test_cli.py::test_target_case FAILED [ 50%]",
        "=================================== FAILURES ===================================",
        "________________________ test_target_case ________________________",
        "E       assert left == right",
        "tests/test_cli.py:42: AssertionError",
        "later summary noise",
    ]


def test_fault_marker_avoids_false_positive_substrings() -> None:
    module = _load_run_logged_module()

    assert (
        module._looks_like_fault_line(
            "test tests::process_metrics_prime_no_panic ... ok"
        )
        is False
    )
    assert module._looks_like_fault_line("Traceback (most recent call last):") is True
