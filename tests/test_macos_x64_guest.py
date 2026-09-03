"""Argv construction and exit-code contract for the dockur/macos guest helper.

`ci/macos_x64_guest.py` never runs a real ssh/docker session under test --
these tests pin the exact argv it builds (soldr#3071 groundwork for the
macos-* runner removal), plus exit-code propagation via a fake
`subprocess.run`.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest
from conftest import load_script_module

SCRIPT = Path(__file__).parents[1] / "ci" / "macos_x64_guest.py"
guest = load_script_module(SCRIPT, "macos_x64_guest")


def test_docker_run_argv_shape() -> None:
    argv = guest.docker_run_argv(
        name="soldr-macos-x86",
        ssh_port=2222,
        image="ghcr.io/zackees/soldr/macos-x64-guest:ventura",
    )
    assert argv[:4] == ["docker", "run", "-d", "--name"]
    assert "soldr-macos-x86" in argv
    assert "--device=/dev/kvm" in argv
    assert "--device=/dev/net/tun" in argv
    assert argv[argv.index("-p") + 1] == "2222:22"
    assert argv[-1] == "ghcr.io/zackees/soldr/macos-x64-guest:ventura"
    assert argv[-3:-1] == ["--stop-timeout", "120"]


def test_docker_run_argv_mounts_guest_storage_for_dev() -> None:
    argv = guest.docker_run_argv(
        name="soldr-macos-x86",
        ssh_port=2222,
        image="dockurr/macos:latest",
        mount=["-v", "/home/dev/storage:/storage"],
    )
    assert "-v" in argv
    assert "/home/dev/storage:/storage" in argv


def test_ssh_argv_shared_options() -> None:
    argv = guest.ssh_argv(2222)
    assert argv[:2] == ["ssh", "-p"]
    assert argv[2] == "2222"
    assert "-o" in argv
    assert "StrictHostKeyChecking=no" in argv
    assert "BatchMode=yes" in argv
    assert argv[-1] == "runner@localhost"


def test_ssh_argv_includes_identity_when_provided() -> None:
    argv = guest.ssh_argv(2222, identity="/tmp/guest_key")
    assert "-i" in argv
    assert "/tmp/guest_key" in argv


def test_rsync_argv_sync_in_trails_host_source_with_slash() -> None:
    argv = guest.rsync_argv(
        host_dir="/repo/checkout",
        guest_dir="/Users/runner/work/ws",
        port=2222,
        direction="in",
    )
    assert argv[0] == "rsync"
    assert argv[-2] == "/repo/checkout/"
    assert argv[-1] == "runner@localhost:/Users/runner/work/ws"


def test_rsync_argv_sync_out_trails_guest_source_with_slash() -> None:
    argv = guest.rsync_argv(
        host_dir="/repo/target",
        guest_dir="/Users/runner/work/ws/target",
        port=2222,
        direction="out",
    )
    assert argv[-2] == "runner@localhost:/Users/runner/work/ws/target/"
    assert argv[-1] == "/repo/target"


def test_rsync_argv_rejects_unknown_direction() -> None:
    with pytest.raises(ValueError, match="unsupported sync direction"):
        guest.rsync_argv(host_dir="/a", guest_dir="/b", port=2222, direction="sideways")


def test_scp_fallback_argv_shape_matches_direction() -> None:
    into_guest = guest.scp_fallback_argv(
        host_dir="/repo", guest_dir="/Users/runner/work/ws", port=2222, direction="in"
    )
    assert into_guest[:2] == ["scp", "-r"]
    assert into_guest[-2:] == ["/repo", "runner@localhost:/Users/runner/work/ws"]

    out_of_guest = guest.scp_fallback_argv(
        host_dir="/repo", guest_dir="/Users/runner/work/ws", port=2222, direction="out"
    )
    assert out_of_guest[-2:] == ["runner@localhost:/Users/runner/work/ws", "/repo"]


def test_remote_command_string_quotes_env_and_cwd() -> None:
    remote = guest.remote_command_string(
        ["echo", "hi there"], cwd="/Users/runner/work/ws", env={"SOLDR_FOO": "a b"}
    )
    assert remote.startswith("set -o pipefail; ")
    assert "cd /Users/runner/work/ws &&" in remote
    assert "SOLDR_FOO='a b'" in remote
    assert "'hi there'" in remote


def test_remote_command_string_requires_a_command() -> None:
    with pytest.raises(ValueError, match="remote command is required"):
        guest.remote_command_string([], cwd=None, env={})


def test_parse_env_pairs_splits_on_first_equals() -> None:
    assert guest.parse_env_pairs(["A=1", "B=two=parts"]) == {"A": "1", "B": "two=parts"}


def test_parse_env_pairs_rejects_missing_equals() -> None:
    with pytest.raises(ValueError, match="malformed --env value"):
        guest.parse_env_pairs(["NOVALUE"])


class _FakeCompleted:  # pylint: disable=too-few-public-methods
    def __init__(self, returncode: int) -> None:
        self.returncode = returncode
        self.stderr = ""


def test_exec_propagates_the_remote_exit_code(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(argv: list[str], **kwargs: Any) -> _FakeCompleted:
        captured["argv"] = argv
        captured["kwargs"] = kwargs
        return _FakeCompleted(returncode=17)

    monkeypatch.setattr(guest.subprocess, "run", fake_run)
    parser = guest.build_parser()
    args = parser.parse_args(["exec", "--", "false"])
    status = guest.cmd_exec(args)
    assert status == 17
    assert captured["kwargs"].get("check") is False
    assert captured["argv"][0] == "ssh"
    assert captured["argv"][-1].endswith("false")


def test_exec_strips_a_single_leading_remainder_delimiter(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(guest.subprocess, "run", lambda *a, **k: _FakeCompleted(0))
    parser = guest.build_parser()
    args = parser.parse_args(["exec", "--cwd", "/x", "--", "/usr/bin/true"])
    assert guest.cmd_exec(args) == 0


def test_exec_without_a_command_is_a_usage_error(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(guest.subprocess, "run", lambda *a, **k: _FakeCompleted(0))
    parser = guest.build_parser()
    args = parser.parse_args(["exec"])
    assert guest.cmd_exec(args) == 2


def test_preflight_reports_instructions_when_kvm_is_unusable(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(guest.os, "access", lambda *_a, **_k: False)
    parser = guest.build_parser()
    args = parser.parse_args(["preflight"])
    status = guest.cmd_preflight(args)
    assert status == 1
    assert "udev" in capsys.readouterr().err
