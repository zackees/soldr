"""Execution-architecture contract for packaged target commands (#2968, #3071)."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = Path(__file__).parents[1] / ".github" / "scripts" / "run_target_command.py"
runner = load_script_module(SCRIPT, "run_target_command")


def test_native_execution_leaves_packaged_target_command_unchanged() -> None:
    assert runner.command_argv("native", ["/artifact/soldr", "--version"]) == [
        "/artifact/soldr",
        "--version",
    ]


def test_rosetta_execution_prefixes_every_packaged_target_command() -> None:
    assert runner.command_argv("x86_64-rosetta", ["/artifact/soldr", "--version"]) == [
        "arch",
        "-x86_64",
        "/artifact/soldr",
        "--version",
    ]


def test_dockur_execution_has_no_static_argv_rewrite() -> None:
    with pytest.raises(ValueError, match="dockur_exec_argv"):
        runner.command_argv("x86_64-dockur", ["/artifact/soldr"])


def test_only_argparse_leading_delimiter_is_removed_from_target_command() -> None:
    assert runner.strip_remainder_delimiter(
        ["--", "/artifact/soldr", "--", "--json"]
    ) == [
        "/artifact/soldr",
        "--",
        "--json",
    ]


@pytest.mark.parametrize("execution", ["unknown", "arm64-rosetta"])
def test_unknown_execution_mode_fails_explicitly(execution: str) -> None:
    with pytest.raises(ValueError, match="unsupported target execution mode"):
        runner.command_argv(execution, ["/artifact/soldr"])


# ---------- x86_64-dockur path mapping (soldr#3071) ----------


def test_parse_path_map_sorts_longest_host_prefix_first() -> None:
    mappings = runner.parse_path_map(
        "/repo=/Users/runner/work/ws;/repo/tmp=/Users/runner/work/tmp"
    )
    assert mappings == [
        ("/repo/tmp", "/Users/runner/work/tmp"),
        ("/repo", "/Users/runner/work/ws"),
    ]


def test_parse_path_map_ignores_blank_entries() -> None:
    assert runner.parse_path_map(" ;/a=/b; ") == [("/a", "/b")]


def test_parse_path_map_rejects_entries_without_equals() -> None:
    with pytest.raises(ValueError, match="malformed path map entry"):
        runner.parse_path_map("/a")


def test_map_path_prefers_the_longest_matching_prefix() -> None:
    mappings = [("/repo/tmp", "/g/tmp"), ("/repo", "/g/ws")]
    assert runner.map_path("/repo/tmp/build/out", mappings) == "/g/tmp/build/out"
    assert runner.map_path("/repo/src/lib.rs", mappings) == "/g/ws/src/lib.rs"


def test_map_path_matches_the_bare_prefix_exactly() -> None:
    assert runner.map_path("/repo", [("/repo", "/g/ws")]) == "/g/ws"


def test_map_path_leaves_unmatched_values_untouched() -> None:
    assert runner.map_path("--json", [("/repo", "/g/ws")]) == "--json"
    assert runner.map_path("/other/dir", [("/repo", "/g/ws")]) == "/other/dir"


def test_map_host_paths_rewrites_flag_equals_path_forms() -> None:
    mappings = [("/repo", "/Users/runner/work/ws")]
    argv = runner.map_host_paths(
        ["--extract-to=/repo/target/extract", "--message-format", "json-pretty"],
        mappings,
    )
    assert argv == [
        "--extract-to=/Users/runner/work/ws/target/extract",
        "--message-format",
        "json-pretty",
    ]


def test_map_host_paths_rewrites_bare_positional_paths() -> None:
    mappings = [("/repo", "/Users/runner/work/ws")]
    argv = runner.map_host_paths(["/repo/artifact/soldr", "--version"], mappings)
    assert argv == ["/Users/runner/work/ws/artifact/soldr", "--version"]


def test_map_host_paths_leaves_non_path_args_untouched() -> None:
    mappings = [("/repo", "/Users/runner/work/ws")]
    argv = runner.map_host_paths(
        ["rustc", "-vV", "--target=x86_64-apple-darwin"], mappings
    )
    assert argv == ["rustc", "-vV", "--target=x86_64-apple-darwin"]


def test_forwarded_env_matches_declared_prefixes_only() -> None:
    mappings = [("/repo", "/g/ws")]
    environ = {
        "SOLDR_CACHE_DIR": "/repo/.cache",
        "NEXTEST_PROFILE": "target-run",
        "CARGO": "/repo/cargo",
        "RUSTC": "/repo/rustc",
        "RUSTUP_TOOLCHAIN": "1.95.0",
        "REPLAY_PARTITION": "hash:1/1",
        "TMPDIR": "/repo/tmp",
        "PATH": "/usr/bin",
        "HOME": "/home/runner",
    }
    result = runner.forwarded_env(environ, mappings)
    assert result == {
        "SOLDR_CACHE_DIR": "/g/ws/.cache",
        "NEXTEST_PROFILE": "target-run",
        "CARGO": "/g/ws/cargo",
        "RUSTC": "/g/ws/rustc",
        "RUSTUP_TOOLCHAIN": "1.95.0",
        "REPLAY_PARTITION": "hash:1/1",
        "TMPDIR": "/g/ws/tmp",
    }


def test_dockur_exec_argv_builds_the_guest_script_invocation() -> None:
    argv = runner.dockur_exec_argv(
        ["/g/ws/artifact/soldr", "--version"],
        cwd="/g/ws",
        env={"SOLDR_CACHE_DIR": "/g/ws/.cache"},
        guest_script=Path("/repo/ci/macos_x64_guest.py"),
        python_exe="python3",
    )
    assert argv == [
        "python3",
        "/repo/ci/macos_x64_guest.py",
        "exec",
        "--cwd",
        "/g/ws",
        "--env",
        "SOLDR_CACHE_DIR=/g/ws/.cache",
        "--",
        "/g/ws/artifact/soldr",
        "--version",
    ]


def test_dockur_preflight_argv_runs_usr_bin_true_in_the_guest() -> None:
    argv = runner.dockur_preflight_argv(
        guest_script=Path("/repo/ci/macos_x64_guest.py"), python_exe="python3"
    )
    assert argv == [
        "python3",
        "/repo/ci/macos_x64_guest.py",
        "exec",
        "--",
        "/usr/bin/true",
    ]


def test_run_dockur_maps_command_cwd_and_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        runner, "GUEST_SCRIPT", Path("/repo/ci/macos_x64_guest.py"), raising=False
    )
    environ = {
        "SOLDR_DOCKUR_PATH_MAP": "/repo=/Users/runner/work/ws",
        "GITHUB_WORKSPACE": "/repo",
        "SOLDR_BIN": "/repo/artifact/soldr",
    }
    argv = runner._run_dockur(  # pylint: disable=protected-access
        ["/repo/artifact/soldr", "toolchain", "ensure"],
        preflight=False,
        environ=environ,
    )
    assert "--cwd" in argv
    assert argv[argv.index("--cwd") + 1] == "/Users/runner/work/ws"
    assert argv[-3:] == ["/Users/runner/work/ws/artifact/soldr", "toolchain", "ensure"]
    assert "--env" in argv
    env_index = argv.index("--env")
    assert argv[env_index + 1] == "SOLDR_BIN=/Users/runner/work/ws/artifact/soldr"


def test_run_dockur_preflight_ignores_missing_path_map() -> None:
    argv = runner._run_dockur(  # pylint: disable=protected-access
        [], preflight=True, environ={}
    )
    assert argv[-2:] == ["--", "/usr/bin/true"]


def test_run_dockur_requires_path_map_when_not_preflighting() -> None:
    with pytest.raises(ValueError, match="SOLDR_DOCKUR_PATH_MAP"):
        runner._run_dockur(  # pylint: disable=protected-access
            ["/repo/artifact/soldr"],
            preflight=False,
            environ={"GITHUB_WORKSPACE": "/repo"},
        )


def test_run_dockur_requires_github_workspace_when_not_preflighting() -> None:
    with pytest.raises(ValueError, match="GITHUB_WORKSPACE"):
        runner._run_dockur(  # pylint: disable=protected-access
            ["/repo/artifact/soldr"],
            preflight=False,
            environ={"SOLDR_DOCKUR_PATH_MAP": "/repo=/g/ws"},
        )
