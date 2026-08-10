from __future__ import annotations

from ci import lint as ci_lint


def test_main_runs_lint_commands_through_running_process_cli(monkeypatch) -> None:
    commands: list[list[str]] = []
    monkeypatch.setattr(
        ci_lint,
        "repo_python",
        lambda: ci_lint.ROOT / ".venv" / "Scripts" / "python.exe",
    )
    monkeypatch.setattr(ci_lint, "cargo_command", lambda *args: ["cargo", *args])
    monkeypatch.setattr(ci_lint, "load_env_helpers", lambda: (lambda: None, lambda: {}))
    monkeypatch.setattr(
        ci_lint,
        "run",
        lambda cmd: commands.append(list(cmd)) or 0,
    )

    result = ci_lint.main()

    python = str(ci_lint.ROOT / ".venv" / "Scripts" / "python.exe")
    timeout = str(ci_lint.DEFAULT_COMMAND_TIMEOUT_SECONDS)
    assert result == 0
    assert commands == [
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.version_check",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.spawn_path_guard",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.docker_manifest_guard",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.jemalloc_guard",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.cross_compiler_guard",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            "cargo",
            "fmt",
            "--all",
            # Verify, never rewrite: plain `cargo fmt --all` exits 0 whether or
            # not it changed anything, so it could never fail CI and it
            # silently rewrote contributors' working trees. See #694.
            "--",
            "--check",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ruff",
            "check",
            "--fix",
            "src",
            "tests",
            "ci",
        ],
        [
            python,
            "-m",
            "running_process.cli",
            "--timeout",
            timeout,
            "--",
            python,
            "-m",
            "ci.lint_python.keyboard_interrupt_checker",
            # Scoped to `src`: the rule is about library code, where an
            # interrupt on a worker thread has to reach the main thread to be
            # seen at all.
            "src",
            "--exclude",
            ".venv",
            "venv",
            "dist",
            ".build",
        ],
    ]
