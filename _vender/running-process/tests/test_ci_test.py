from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

from ci import test as ci_test


@pytest.fixture(autouse=True)
def _isolate_ci_env(monkeypatch: pytest.MonkeyPatch) -> None:
    """Make every test in this module hermetic w.r.t. CI diagnostics envs.

    `RUNNING_PROCESS_TEST_NOCAPTURE=1` is set by the GH Actions
    workflows so nextest forwards println!s through. When this env var
    leaks into the test process, `ci/test.py` appends `--no-capture`
    and the static command-shape assertions in this module no longer match.
    """
    monkeypatch.delenv("RUNNING_PROCESS_TEST_NOCAPTURE", raising=False)
    monkeypatch.setattr(ci_test, "_ensure_nextest_installed", lambda: True)


def _expected_build_testbins_cmd() -> list[str]:
    """Fixtures are built once up front so tests only look them up (#747)."""
    return ["cargo", "build", "-p", "testbins"]


def _expected_build_tokio_fixture_cmd() -> list[str]:
    """The console-subscriber fixture is a separate workspace (#788).

    Built with `--cfg tokio_unstable` via `extra_env`, which cargo cannot
    scope to one crate inside a workspace.
    """
    return ["cargo", "build", "--manifest-path", "testbins-tokio/Cargo.toml"]


def _expected_cargo_build_tests_cmd() -> list[str]:
    """Build the expected unsupervised nextest --no-run command."""
    return ["cargo", "nextest", "run", "--workspace", "--no-run"]


def _expected_cargo_test_cmd(python: str) -> list[str]:
    """Build the expected supervised nextest command for the current platform."""
    timeout = (
        str(ci_test.WINDOWS_RUST_TEST_TIMEOUT_SECONDS)
        if ci_test.sys.platform == "win32"
        else str(ci_test.DEFAULT_RUST_TEST_TIMEOUT_SECONDS)
    )
    cmd = [
        python,
        "-m",
        "running_process.cli",
        "--timeout",
        timeout,
        "--",
        "cargo",
        "nextest",
        "run",
        "--workspace",
    ]
    if ci_test.sys.platform == "win32":
        cmd += ["--test-threads", "1"]
    return cmd


def _expected_seam_test_cmd(python: str) -> list[str]:
    """Build the expected supervised test-seams nextest pass (#433 R4)."""
    timeout = (
        str(ci_test.WINDOWS_RUST_TEST_TIMEOUT_SECONDS)
        if ci_test.sys.platform == "win32"
        else str(ci_test.DEFAULT_RUST_TEST_TIMEOUT_SECONDS)
    )
    cmd = [
        python,
        "-m",
        "running_process.cli",
        "--timeout",
        timeout,
        "--",
        "cargo",
        "nextest",
        "run",
        "-p",
        "running-process",
        "--features",
        "test-seams",
        "--test",
        "broker",
        "-E",
        "test(fake_backend)",
    ]
    if ci_test.sys.platform == "win32":
        cmd += ["--test-threads", "1"]
    return cmd


def test_prune_invalid_profraw_preserves_evidence_before_removal(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    profile_dir = tmp_path / "profiles"
    bad_dir = tmp_path / "logs" / "bad-profraw"
    profile_dir.mkdir()
    valid = profile_dir / "valid.profraw"
    invalid = profile_dir / "nested" / "invalid.profraw"
    invalid.parent.mkdir()
    valid.write_bytes(b"valid-profile")
    invalid.write_bytes(b"rejected-profile")

    fake_profdata = tmp_path / "fake_llvm_profdata.py"
    fake_profdata.write_text(
        "\n".join(
            [
                "import pathlib",
                "import sys",
                "if sys.argv[1] == '--version':",
                "    print('LLVM fake-profdata 21.1.8')",
                "    raise SystemExit(0)",
                "profile = pathlib.Path(sys.argv[-1])",
                "raise SystemExit(23 if profile.name.startswith('invalid') else 0)",
            ]
        ),
        encoding="utf-8",
    )
    monkeypatch.setenv("RUNNER_OS", "fake-linux")
    monkeypatch.setenv("GITHUB_RUN_ID", "123456")
    monkeypatch.setenv("GITHUB_SHA", "deadbeef")

    count = ci_test._prune_invalid_profraw(
        profile_dir,
        bad_dir=bad_dir,
        profdata_command=[sys.executable, str(fake_profdata)],
    )

    assert count == 1
    assert valid.read_bytes() == b"valid-profile"
    assert not invalid.exists()
    assert (bad_dir / "nested" / "invalid.profraw").read_bytes() == b"rejected-profile"
    manifest = json.loads((bad_dir / "manifest.json").read_text(encoding="utf-8"))
    assert manifest["llvm_version"] == "LLVM fake-profdata 21.1.8"
    assert manifest["runner_os"] == "fake-linux"
    assert manifest["github_run_id"] == "123456"
    assert manifest["github_commit"] == "deadbeef"
    assert manifest["rejected_profiles"] == [
        {
            "filename": "invalid.profraw",
            "original_path": str(invalid),
            "preserved_path": (
                "nested\\invalid.profraw"
                if sys.platform == "win32"
                else "nested/invalid.profraw"
            ),
            "probe_command": [
                sys.executable,
                str(fake_profdata),
                "show",
                str(invalid),
            ],
            "probe_returncode": 23,
            "probe_signal": None,
            "probe_stderr": "",
            "size_bytes": len(b"rejected-profile"),
        }
    ]


def test_main_runs_pytest_through_running_process_cli(monkeypatch) -> None:
    commands: list[list[str]] = []
    fake_python = Path("/tmp/fake-venv/bin/python")

    monkeypatch.delenv(ci_test.GITHUB_ACTIONS_ENV, raising=False)
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.delenv("RUNNING_PROCESS_LIVE_TESTS", raising=False)
    monkeypatch.delenv(ci_test.SKIP_LINUX_DOCKER_ENV, raising=False)
    monkeypatch.setattr(ci_test.sys, "executable", str(fake_python))
    monkeypatch.setattr(ci_test, "cargo_command", lambda *args: ["cargo", *args])
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(
        ci_test,
        "run",
        lambda cmd, extra_env=None: commands.append(list(cmd)) or 0,
    )
    monkeypatch.setattr(
        ci_test,
        "run_live",
        lambda cmd, extra_env=None: commands.append(list(cmd)) or 0,
    )

    result = ci_test.main([])

    python = str(fake_python)
    pytest_timeout = str(ci_test.DEFAULT_PYTEST_TIMEOUT_SECONDS)
    linux_timeout = str(ci_test.DEFAULT_LINUX_TEST_TIMEOUT_SECONDS)
    assert result == 0
    assert commands == [
        _expected_build_testbins_cmd(),
        _expected_build_tokio_fixture_cmd(),
        _expected_cargo_build_tests_cmd(),
        _expected_cargo_test_cmd(python),
        _expected_seam_test_cmd(python),
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            pytest_timeout,
            "--",
            python,
            "-m",
            "pytest",
            "-vv",
            "-m",
            "not live",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            linux_timeout,
            "--",
            python,
            "-m",
            "ci.linux_docker",
            "all",
            "--output-dir",
            str(ci_test.ROOT / "linux"),
        ],
    ]


def test_main_skips_dev_wheel_reinstall_when_running_under_cli(monkeypatch) -> None:
    called = False

    def fake_ensure_dev_wheel(*args, **kwargs):
        del args, kwargs
        nonlocal called
        called = True
        return "built"

    monkeypatch.setenv(ci_test.IN_RUNNING_PROCESS_ENV, ci_test.IN_RUNNING_PROCESS_VALUE)
    monkeypatch.delenv("RUNNING_PROCESS_LIVE_TESTS", raising=False)
    monkeypatch.setattr(ci_test, "cargo_command", lambda *args: ["cargo", *args])
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", fake_ensure_dev_wheel)
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(ci_test, "run", lambda cmd, extra_env=None: 0)
    monkeypatch.setattr(ci_test, "run_live", lambda cmd: 0)

    result = ci_test.main([])

    assert result == 0
    assert called is False


def test_main_skips_linux_docker_preflight_on_github_actions(monkeypatch) -> None:
    commands: list[list[str]] = []
    fake_python = Path("/tmp/fake-venv/bin/python")

    monkeypatch.setenv(ci_test.GITHUB_ACTIONS_ENV, "true")
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.delenv("RUNNING_PROCESS_LIVE_TESTS", raising=False)
    monkeypatch.setattr(ci_test.sys, "executable", str(fake_python))
    monkeypatch.setattr(ci_test, "cargo_command", lambda *args: ["cargo", *args])
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(
        ci_test, "run", lambda cmd, extra_env=None: commands.append(list(cmd)) or 0
    )
    monkeypatch.setattr(
        ci_test, "run_live", lambda cmd: commands.append(list(cmd)) or 0
    )

    result = ci_test.main([])

    python = str(fake_python)
    pytest_timeout = str(ci_test.DEFAULT_PYTEST_TIMEOUT_SECONDS)
    assert result == 0
    assert commands == [
        _expected_build_testbins_cmd(),
        _expected_build_tokio_fixture_cmd(),
        _expected_cargo_build_tests_cmd(),
        _expected_cargo_test_cmd(python),
        _expected_seam_test_cmd(python),
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            pytest_timeout,
            "--",
            python,
            "-m",
            "pytest",
            "-vv",
            "-m",
            "not live",
        ],
    ]


def test_main_skips_linux_docker_preflight_when_env_requests_it(monkeypatch) -> None:
    commands: list[list[str]] = []
    fake_python = Path("/tmp/fake-venv/bin/python")

    monkeypatch.setenv(ci_test.SKIP_LINUX_DOCKER_ENV, "1")
    monkeypatch.delenv(ci_test.GITHUB_ACTIONS_ENV, raising=False)
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.delenv("RUNNING_PROCESS_LIVE_TESTS", raising=False)
    monkeypatch.setattr(ci_test.sys, "executable", str(fake_python))
    monkeypatch.setattr(ci_test, "cargo_command", lambda *args: ["cargo", *args])
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(
        ci_test, "run", lambda cmd, extra_env=None: commands.append(list(cmd)) or 0
    )
    monkeypatch.setattr(
        ci_test, "run_live", lambda cmd: commands.append(list(cmd)) or 0
    )

    result = ci_test.main([])

    python = str(fake_python)
    pytest_timeout = str(ci_test.DEFAULT_PYTEST_TIMEOUT_SECONDS)
    assert result == 0
    assert commands == [
        _expected_build_testbins_cmd(),
        _expected_build_tokio_fixture_cmd(),
        _expected_cargo_build_tests_cmd(),
        _expected_cargo_test_cmd(python),
        _expected_seam_test_cmd(python),
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            pytest_timeout,
            "--",
            python,
            "-m",
            "pytest",
            "-vv",
            "-m",
            "not live",
        ],
    ]


def test_parse_args_converts_target_and_selector_to_pytest_k_expr(monkeypatch) -> None:
    monkeypatch.delenv("RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS", raising=False)

    pytest_args, require_symbols, coverage, live_only = ci_test.parse_args(
        ["tests/test_pty_support.py", "timeout_does_not_arm_next_expect"]
    )

    assert pytest_args == [
        "tests/test_pty_support.py",
        "-k",
        "timeout_does_not_arm_next_expect",
    ]
    assert require_symbols is False
    assert coverage is False
    assert live_only is False


def test_parse_args_preserves_explicit_pytest_flags() -> None:
    pytest_args, require_symbols, coverage, live_only = ci_test.parse_args(
        ["tests/test_pty_support.py", "-k", "timeout_does_not_arm_next_expect", "-ra"]
    )

    assert pytest_args == [
        "tests/test_pty_support.py",
        "-k",
        "timeout_does_not_arm_next_expect",
        "-ra",
    ]
    assert require_symbols is False
    assert coverage is False
    assert live_only is False


def test_parse_args_tracks_no_skip_without_mutating_env(monkeypatch) -> None:
    monkeypatch.delenv("RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS", raising=False)

    pytest_args, require_symbols, coverage, live_only = ci_test.parse_args(
        ["--no-skip", "tests/test_version.py"]
    )

    assert pytest_args == ["tests/test_version.py"]
    assert require_symbols is True
    assert coverage is False
    assert live_only is False
    assert "RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS" not in os.environ


def test_parse_args_tracks_live_only_flag() -> None:
    pytest_args, require_symbols, coverage, live_only = ci_test.parse_args(
        ["--live-only", "tests/test_pty_support.py"]
    )

    assert pytest_args == ["tests/test_pty_support.py"]
    assert require_symbols is False
    assert coverage is False
    assert live_only is True


def test_pytest_exit_is_acceptable_only_allows_no_tests_for_targeted_runs() -> None:
    assert ci_test._pytest_exit_is_acceptable(0, []) is True
    assert ci_test._pytest_exit_is_acceptable(5, []) is False
    assert ci_test._pytest_exit_is_acceptable(5, ["tests/test_pty_support.py"]) is True


def test_main_allows_targeted_live_selection_with_no_matching_tests(
    monkeypatch,
) -> None:
    fake_python = Path("/tmp/fake-venv/bin/python")

    monkeypatch.setenv(ci_test.SKIP_LINUX_DOCKER_ENV, "1")
    monkeypatch.setenv("RUNNING_PROCESS_LIVE_TESTS", "1")
    monkeypatch.delenv(ci_test.GITHUB_ACTIONS_ENV, raising=False)
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.setattr(ci_test.sys, "executable", str(fake_python))
    monkeypatch.setattr(ci_test, "cargo_command", lambda *args: ["cargo", *args])
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(ci_test, "run", lambda cmd, extra_env=None: 0)
    monkeypatch.setattr(ci_test, "run_live", lambda cmd: 5)

    result = ci_test.main(["tests/test_pty_support.py"])

    assert result == 0


def test_main_live_only_runs_only_live_pytest_through_cli(monkeypatch) -> None:
    commands: list[list[str]] = []
    fake_python = Path("/tmp/fake-venv/bin/python")

    monkeypatch.delenv(ci_test.GITHUB_ACTIONS_ENV, raising=False)
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.delenv(ci_test.SKIP_LINUX_DOCKER_ENV, raising=False)
    monkeypatch.delenv("RUNNING_PROCESS_LIVE_TESTS", raising=False)
    monkeypatch.setattr(ci_test.sys, "executable", str(fake_python))
    monkeypatch.setattr(ci_test, "cargo_command", lambda *args: ["cargo", *args])
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(
        ci_test, "run", lambda cmd, extra_env=None: commands.append(list(cmd)) or 0
    )
    monkeypatch.setattr(
        ci_test, "run_live", lambda cmd: commands.append(list(cmd)) or 0
    )

    result = ci_test.main(["--live-only", "tests/test_pty_support.py"])

    python = str(fake_python)
    pytest_timeout = str(ci_test.DEFAULT_PYTEST_TIMEOUT_SECONDS)
    assert result == 0
    assert os.environ["RUNNING_PROCESS_LIVE_TESTS"] == "1"
    assert commands == [
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            pytest_timeout,
            "--",
            python,
            "-m",
            "pytest",
            "-vv",
            "-m",
            "live",
            "tests/test_pty_support.py",
        ],
    ]


def test_main_builds_release_wheel_before_live_tests_when_symbols_required(
    monkeypatch,
) -> None:
    commands: list[list[str]] = []
    fake_python = Path("/tmp/fake-venv/bin/python")

    monkeypatch.delenv(ci_test.GITHUB_ACTIONS_ENV, raising=False)
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.delenv(ci_test.SKIP_LINUX_DOCKER_ENV, raising=False)
    monkeypatch.setenv("RUNNING_PROCESS_LIVE_TESTS", "1")
    monkeypatch.setenv("RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS", "0")
    monkeypatch.setattr(ci_test.sys, "executable", str(fake_python))
    monkeypatch.setattr(ci_test.sys, "platform", "win32")
    monkeypatch.setattr(ci_test, "cargo_command", lambda *args: ["cargo", *args])
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(
        ci_test, "run", lambda cmd, extra_env=None: commands.append(list(cmd)) or 0
    )
    monkeypatch.setattr(
        ci_test, "run_live", lambda cmd: commands.append(list(cmd)) or 0
    )

    result = ci_test.main(["--no-skip"])

    python = str(fake_python)
    pytest_timeout = str(ci_test.DEFAULT_PYTEST_TIMEOUT_SECONDS)
    linux_timeout = str(ci_test.DEFAULT_LINUX_TEST_TIMEOUT_SECONDS)
    release_timeout = str(ci_test.DEFAULT_RELEASE_BUILD_TIMEOUT_SECONDS)
    assert result == 0
    assert os.environ["RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS"] == "1"
    assert commands == [
        _expected_build_testbins_cmd(),
        _expected_build_tokio_fixture_cmd(),
        _expected_cargo_build_tests_cmd(),
        _expected_cargo_test_cmd(python),
        _expected_seam_test_cmd(python),
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            pytest_timeout,
            "--",
            python,
            "-m",
            "pytest",
            "-vv",
            "-m",
            "not live",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            linux_timeout,
            "--",
            python,
            "-m",
            "ci.linux_docker",
            "all",
            "--output-dir",
            str(ci_test.ROOT / "linux"),
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            release_timeout,
            "--",
            python,
            "build.py",
            "--release",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            pytest_timeout,
            "--",
            python,
            "-m",
            "pytest",
            "-vv",
            "-m",
            "live",
        ],
    ]


def _fake_profdata(tmp_path: Path, body: str) -> list[str]:
    """A stand-in llvm-profdata whose behavior the test dictates."""
    script = tmp_path / "fake_profdata.py"
    script.write_text(body, encoding="utf-8")
    return [sys.executable, str(script)]


def test_describe_abnormal_exit_names_a_signal_death() -> None:
    # POSIX reports a signal death as the negated signal number.
    described = ci_test.describe_abnormal_exit(-4)
    assert described is not None
    assert "signal 4" in described
    assert "SIGILL" in described


def test_describe_abnormal_exit_names_a_windows_fault() -> None:
    # 0xC000001D is STATUS_ILLEGAL_INSTRUCTION, the #626 failure on Windows.
    described = ci_test.describe_abnormal_exit(0xC000001D)
    assert described is not None
    assert "STATUS_ILLEGAL_INSTRUCTION" in described


def test_describe_abnormal_exit_ignores_ordinary_exits() -> None:
    # A tool that ran and objected is not a broken tool; conflating the two is
    # the misreading #626 is about.
    assert ci_test.describe_abnormal_exit(0) is None
    assert ci_test.describe_abnormal_exit(1) is None
    assert ci_test.describe_abnormal_exit(23) is None


def test_llvm_profdata_preflight_accepts_a_working_binary(tmp_path: Path) -> None:
    command = _fake_profdata(
        tmp_path,
        "\n".join(
            [
                "import sys",
                "if sys.argv[1] == '--version':",
                "    print('LLVM fake-profdata 21.1.8')",
                "    raise SystemExit(0)",
                # A merge with no inputs objects; that is a normal exit and
                # must not be mistaken for a broken toolchain.
                "raise SystemExit(1)",
            ]
        ),
    )
    assert ci_test.llvm_profdata_preflight(profdata_command=command) is None


def test_llvm_profdata_preflight_reports_a_faulting_binary(tmp_path: Path) -> None:
    command = _fake_profdata(
        tmp_path,
        "\n".join(
            [
                "import os",
                "import signal",
                "import sys",
                "if sys.argv[1] == '--version':",
                "    print('LLVM fake-profdata 21.1.8')",
                "    raise SystemExit(0)",
                # Die the way #626 dies: a fault during merge, not an exit.
                "if sys.platform == 'win32':",
                "    os._exit(-1073741795)",  # 0xC000001D as a signed int
                "os.kill(os.getpid(), signal.SIGILL)",
            ]
        ),
    )
    diagnostic = ci_test.llvm_profdata_preflight(profdata_command=command)

    assert diagnostic is not None, "a faulting llvm-profdata must be reported"
    assert "#626" in diagnostic
    # The version must be captured from the probe that worked, so an
    # investigator learns which toolchain produced the fault.
    assert "fake-profdata 21.1.8" in diagnostic
    assert "toolchain/CPU incompatibility" in diagnostic


def test_llvm_profdata_preflight_reports_an_unrunnable_binary(tmp_path: Path) -> None:
    missing = str(tmp_path / "does-not-exist")
    diagnostic = ci_test.llvm_profdata_preflight(profdata_command=[missing])
    assert diagnostic is not None
    assert "cannot execute" in diagnostic


def test_llvm_profdata_preflight_is_silent_when_the_binary_is_absent(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # No llvm-profdata at all is not this check's business: coverage should
    # fail on its own terms rather than on a guess from the preflight.
    monkeypatch.setattr(ci_test, "_find_llvm_profdata", lambda: None)
    assert ci_test.llvm_profdata_preflight() is None


def test_main_aborts_coverage_before_running_tests_when_profdata_faults(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The preflight must gate the suite, not merely exist.

    Its whole value is being consulted *before* the expensive run, so this
    asserts no command was issued at all.
    """
    commands: list[list[str]] = []

    monkeypatch.delenv(ci_test.GITHUB_ACTIONS_ENV, raising=False)
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.delenv("RUNNING_PROCESS_LIVE_TESTS", raising=False)
    monkeypatch.setattr(ci_test, "cargo_command", lambda *args: ["cargo", *args])
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(ci_test, "_ensure_nextest_installed", lambda: True)
    monkeypatch.setattr(
        ci_test,
        "run",
        lambda cmd, extra_env=None: commands.append(list(cmd)) or 0,
    )
    monkeypatch.setattr(
        ci_test,
        "llvm_profdata_preflight",
        lambda *args, **kwargs: "coverage: llvm-profdata is unusable (#626).",
    )

    result = ci_test.main(["--coverage"])

    assert result == 1
    assert commands == [], "the suite must not run once the toolchain is known bad"
    assert "#626" in capsys.readouterr().err


def test_main_runs_coverage_when_the_preflight_passes(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Guard against the preflight blocking a healthy toolchain."""
    commands: list[list[str]] = []

    monkeypatch.delenv(ci_test.GITHUB_ACTIONS_ENV, raising=False)
    monkeypatch.delenv(ci_test.IN_RUNNING_PROCESS_ENV, raising=False)
    monkeypatch.delenv("RUNNING_PROCESS_LIVE_TESTS", raising=False)
    monkeypatch.setenv(ci_test.SKIP_LINUX_DOCKER_ENV, "1")
    monkeypatch.setattr(ci_test, "cargo_command", lambda *args: ["cargo", *args])
    monkeypatch.setattr(ci_test, "ensure_dev_wheel", lambda *args, **kwargs: "built")
    monkeypatch.setattr(ci_test, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(ci_test, "_ensure_nextest_installed", lambda: True)
    monkeypatch.setattr(ci_test, "_prune_invalid_profraw", lambda *a, **k: 0)
    monkeypatch.setattr(ci_test, "llvm_profdata_preflight", lambda *a, **k: None)
    monkeypatch.setattr(
        ci_test,
        "run",
        lambda cmd, extra_env=None: commands.append(list(cmd)) or 0,
    )

    result = ci_test.main(["--coverage"])

    assert result == 0
    assert any(
        "llvm-cov" in " ".join(cmd) for cmd in commands
    ), f"coverage should have run; issued {commands}"
