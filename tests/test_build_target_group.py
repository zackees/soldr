import json
import sys
from pathlib import Path
from unittest import mock

import pytest

# Ensure ci/ is importable
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "ci"))
from build_target_group import GROUPS, resolve_target, compute_plan


class TestGroupExpansion:
    def test_win_mac_musl_expands_to_four_targets(self):
        assert GROUPS["win-mac-musl"] == [
            "win-x64",
            "mac-arm64",
            "linux-x64-musl",
            "linux-arm64-musl",
        ]

    def test_alias_order_is_preserved(self):
        order = GROUPS["win-mac-musl"]
        assert order.index("win-x64") < order.index("mac-arm64")
        assert order.index("mac-arm64") < order.index("linux-x64-musl")
        assert order.index("linux-x64-musl") < order.index("linux-arm64-musl")


class TestAliasResolution:
    @mock.patch("build_target_group.json.load")
    def test_resolve_target_returns_triple(self, mock_json_load):
        mock_json_load.return_value = {
            "win-x64": "x86_64-pc-windows-msvc",
            "mac-arm64": "aarch64-apple-darwin",
            "linux-x64-musl": "x86_64-unknown-linux-musl",
            "linux-arm64-musl": "aarch64-unknown-linux-musl",
        }
        assert resolve_target("win-x64") == "x86_64-pc-windows-msvc"

    @mock.patch("build_target_group.json.load")
    def test_resolve_target_raises_on_unknown(self, mock_json_load):
        mock_json_load.return_value = {}
        with pytest.raises(ValueError, match="Unknown alias 'unknown'"):
            resolve_target("unknown")


class TestArtifactPaths:
    @mock.patch("build_target_group.json.load")
    def test_windows_artifact_has_exe_extension(self, mock_json_load):
        mock_json_load.return_value = {"win-x64": "x86_64-pc-windows-msvc"}
        plan = compute_plan("win-mac-musl", Path("/tmp/dist"), [])
        win_plan = next(p for p in plan if p.alias == "win-x64")
        assert any(str(p).endswith("soldr.exe") for p in win_plan.artifact_paths)
        assert any(str(p).endswith("soldr-daemon.exe") for p in win_plan.artifact_paths)

    @mock.patch("build_target_group.json.load")
    def test_unix_artifact_no_exe_extension(self, mock_json_load):
        mock_json_load.return_value = {"linux-x64-musl": "x86_64-unknown-linux-musl"}
        plan = compute_plan("win-mac-musl", Path("/tmp/dist"), [])
        lin_plan = next(p for p in plan if p.alias == "linux-x64-musl")
        assert not any(p.name.endswith(".exe") for p in lin_plan.artifact_paths)


class TestCommandConstruction:
    @mock.patch("build_target_group.json.load")
    def test_soldr_build_command_format(self, mock_json_load):
        mock_json_load.return_value = {"win-x64": "x86_64-pc-windows-msvc"}
        plan = compute_plan("win-mac-musl", Path("/tmp/dist"), ["--features", "foo"])
        win_plan = next(p for p in plan if p.alias == "win-x64")
        assert win_plan.command == [
            "soldr",
            "build",
            "--release",
            "--target",
            "win-x64",
            "--features",
            "foo",
        ]
