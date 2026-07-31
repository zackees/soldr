from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import time
import types
import unittest
from pathlib import Path
from typing import Any
from unittest import mock


def _load_backend() -> Any:
    path = Path(__file__).parents[1] / "src" / "soldr" / "__init__.py"
    spec = importlib.util.spec_from_file_location("soldr_test_backend", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class Pep517Pyo3PolicyTest(unittest.TestCase):
    def setUp(self) -> None:
        self.backend = _load_backend()
        self.original_query_soldr_root = self.backend._query_soldr_root
        patcher = mock.patch.object(
            self.backend,
            "_query_soldr_root",
            return_value=Path.home() / ".soldr",
        )
        patcher.start()
        self.addCleanup(patcher.stop)
        self.original_hold_build_lease = self.backend._hold_build_lease

        @contextlib.contextmanager
        def no_op_build_lease(_environment):
            yield

        lease_patcher = mock.patch.object(
            self.backend, "_hold_build_lease", no_op_build_lease
        )
        lease_patcher.start()
        self.addCleanup(lease_patcher.stop)

    def test_native_env_never_forces_no_python(self) -> None:
        previous = os.environ.pop("PYO3_NO_PYTHON", None)
        try:
            with mock.patch.dict(
                os.environ, {"SOLDR_PEP517_STABLE_TARGET_DIR": "0"}, clear=False
            ):
                env = self.backend._prep_env()
        finally:
            if previous is not None:
                os.environ["PYO3_NO_PYTHON"] = previous
        self.assertNotIn("PYO3_NO_PYTHON", env)

    def test_local_profile_defaults_to_explicit_fast_dev(self) -> None:
        """Each fast-dev default is applied *unless* the project sets that field.

        soldr#2004: this used to assert `CARGO_PROFILE_DEV_DEBUG` verbatim and
        failed on `main`. The product was right and the test was wrong --
        `_prep_env` deliberately skips a variable whose `[profile.dev]` field
        the project already sets, because a project-level setting is documented
        to win. soldr's own Cargo.toml carries `debug = "line-tables-only"`, so
        running the suite from this repo is exactly the case that must omit it.

        Asserting the rule instead of a snapshot makes the test independent of
        whatever manifest it happens to run beside.
        """
        with mock.patch.dict(
            os.environ,
            {"SOLDR_PEP517_STABLE_TARGET_DIR": "0"},
            clear=False,
        ):
            env = self.backend._prep_env()
        project_fields = self.backend._project_dev_profile_options()
        for key, (
            cargo_key,
            default,
        ) in self.backend._FAST_DEV_PROFILE_DEFAULTS.items():
            if cargo_key in project_fields:
                self.assertNotIn(
                    key,
                    env,
                    f"{key} must be left to the project, which sets [profile.dev] {cargo_key}",
                )
            else:
                self.assertEqual(env[key], default, f"{key} should carry its default")
        self.assertEqual(self.backend._profile_args(None), ["--profile", "dev"])
        self.assertEqual(env["SOLDR_PEP517_LINKER"], "auto")

    def test_every_fast_dev_default_applies_when_the_project_sets_none(self) -> None:
        """The default path itself, which the test above cannot reach from here.

        soldr#2004: with soldr's own `[profile.dev] debug` in the way, the
        assertions above skip that field entirely. Pinning the pure-default
        behaviour needs a project that specifies nothing, so this stubs the
        reader rather than contriving a temp checkout.
        """
        with mock.patch.object(
            self.backend, "_project_dev_profile_options", return_value={}
        ):
            with mock.patch.dict(
                os.environ,
                {"SOLDR_PEP517_STABLE_TARGET_DIR": "0"},
                clear=False,
            ):
                env = self.backend._prep_env()
        self.assertEqual(env["CARGO_PROFILE_DEV_OPT_LEVEL"], "0")
        self.assertEqual(env["CARGO_PROFILE_DEV_CODEGEN_UNITS"], "256")
        self.assertEqual(env["CARGO_PROFILE_DEV_DEBUG"], "line-tables-only")
        self.assertEqual(env["CARGO_PROFILE_DEV_LTO"], "false")
        self.assertEqual(env["CARGO_PROFILE_DEV_INCREMENTAL"], "true")

    def test_a_project_field_is_never_overridden_by_the_default(self) -> None:
        """The rule that made the original test fail, asserted directly."""
        with mock.patch.object(
            self.backend,
            "_project_dev_profile_options",
            return_value={"opt-level": "3"},
        ):
            with mock.patch.dict(
                os.environ,
                {"SOLDR_PEP517_STABLE_TARGET_DIR": "0"},
                clear=False,
            ):
                env = self.backend._prep_env()
        self.assertNotIn(
            "CARGO_PROFILE_DEV_OPT_LEVEL",
            env,
            "a project-set field must not be shadowed by soldr's default",
        )
        # Fields the project does not set still get theirs.
        self.assertEqual(env["CARGO_PROFILE_DEV_LTO"], "false")

    def test_caller_profile_and_environment_values_win(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "SOLDR_PEP517_PROFILE": "release",
                "CARGO_PROFILE_DEV_OPT_LEVEL": "1",
                "CARGO_PROFILE_DEV_CODEGEN_UNITS": "32",
                "CARGO_PROFILE_DEV_DEBUG": "2",
                "SOLDR_PEP517_STABLE_TARGET_DIR": "0",
            },
            clear=False,
        ):
            env = self.backend._prep_env()
        self.assertEqual(env["CARGO_PROFILE_DEV_OPT_LEVEL"], "1")
        self.assertEqual(env["CARGO_PROFILE_DEV_CODEGEN_UNITS"], "32")
        self.assertEqual(env["CARGO_PROFILE_DEV_DEBUG"], "2")
        self.assertEqual(
            self.backend._profile_args({"profile": "ci"}), ["--profile", "ci"]
        )

    def test_maturin_child_reuses_configured_target_environment(self) -> None:
        settings = {"profile": "release", "target": "linux-arm64"}
        environment = {
            "CARGO_TARGET_DIR": "/managed/target/linux-arm64/release",
            "SOLDR_PEP517_PROFILE": "release",
        }
        with mock.patch.object(
            self.backend, "_prep_env", return_value=environment
        ) as prep:
            with mock.patch.object(self.backend, "_run_pep517_streaming") as run_child:
                self.backend._maturin_pep517(
                    "build-wheel",
                    config_settings=settings,
                    editable=True,
                )
        prep.assert_called_once_with(settings, editable=True)
        self.assertIs(run_child.call_args.kwargs["env"], environment)

    def test_wheel_build_prints_default_cache_summary(self) -> None:
        calls = []
        build_calls = []

        def fake_run(command, **kwargs):
            calls.append((command, kwargs))
            if command[1] == "session-start":
                payload = {"command": "session-start", "session_id": "pep517-1"}
            else:
                payload = {
                    "command": "session-end",
                    "stats": {
                        "hits": 9,
                        "misses": 3,
                        "hit_rate": 0.75,
                        "time_saved_ms": 1500,
                    },
                }
            return subprocess.CompletedProcess(command, 0, json.dumps(payload), "")

        stream = io.StringIO()
        with mock.patch.object(self.backend, "_prep_env", return_value={}):
            with mock.patch.object(
                self.backend.subprocess, "run", side_effect=fake_run
            ):
                with mock.patch.object(
                    self.backend,
                    "_run_pep517_streaming",
                    side_effect=lambda *args, **kwargs: build_calls.append(
                        (args, kwargs)
                    ),
                ):
                    with mock.patch.object(
                        self.backend.time, "perf_counter", side_effect=[10.0, 12.25]
                    ):
                        with contextlib.redirect_stderr(stream):
                            self.backend._maturin_pep517(
                                "build-wheel", build_label="wheel"
                            )

        self.assertEqual(calls[0][0], ["soldr", "session-start", "--json"])
        self.assertEqual(
            calls[1][0], ["soldr", "session-end", "--id", "pep517-1", "--json"]
        )
        self.assertEqual(build_calls[0][1]["env"]["ZCCACHE_SESSION_ID"], "pep517-1")
        self.assertIn("built wheel in 2.2s", stream.getvalue())
        self.assertIn(
            "cache 9 hits / 3 misses (75.0%) | saved 1.5s",
            stream.getvalue(),
        )

    def test_verbose_build_prints_full_session_stats(self) -> None:
        self.assertEqual(self.backend._stats_mode({"PIP_VERBOSE": "1"}), "full")
        self.assertEqual(self.backend._stats_mode({"SOLDR_PEP517_STATS": "off"}), "off")
        self.assertEqual(
            self.backend._stats_mode(
                {"PIP_VERBOSE": "1", "SOLDR_PEP517_STATS": "short"}
            ),
            "short",
        )

        def fake_run(command, **kwargs):
            payload = (
                {"command": "session-start", "session_id": "pep517-2"}
                if command[1] == "session-start"
                else {
                    "command": "session-end",
                    "stats": {
                        "hits": 1,
                        "misses": 0,
                        "phase_profile": {"staged": {}},
                    },
                }
            )
            return subprocess.CompletedProcess(command, 0, json.dumps(payload), "")

        stream = io.StringIO()
        with mock.patch.object(
            self.backend, "_prep_env", return_value={"SOLDR_PEP517_STATS": "full"}
        ):
            with mock.patch.object(
                self.backend.subprocess, "run", side_effect=fake_run
            ):
                with mock.patch.object(self.backend, "_run_pep517_streaming"):
                    with mock.patch.object(
                        self.backend.time, "perf_counter", side_effect=[1.0, 2.0]
                    ):
                        with contextlib.redirect_stderr(stream):
                            self.backend._maturin_pep517(
                                "build-wheel", build_label="wheel"
                            )

        self.assertIn("soldr PEP 517 details:", stream.getvalue())
        self.assertIn('"phase_profile": {"staged": {}}', stream.getvalue())

    def test_failed_wheel_build_ends_session_without_success_summary(
        self,
    ) -> None:
        calls = []
        stream = io.StringIO()

        def fake_run(command, **kwargs):
            calls.append(command)
            payload = (
                {"command": "session-start", "session_id": "pep517-fail"}
                if command[1] == "session-start"
                else {"command": "session-end", "stats": {"hits": 0, "misses": 1}}
            )
            return subprocess.CompletedProcess(command, 0, json.dumps(payload), "")

        stream = io.StringIO()
        with mock.patch.object(self.backend, "_prep_env", return_value={}):
            with mock.patch.object(
                self.backend.subprocess, "run", side_effect=fake_run
            ):
                with mock.patch.object(
                    self.backend,
                    "_run_pep517_streaming",
                    side_effect=subprocess.CalledProcessError(1, "soldr"),
                ):
                    with contextlib.redirect_stderr(stream):
                        with self.assertRaises(subprocess.CalledProcessError):
                            self.backend._maturin_pep517(
                                "build-wheel", build_label="wheel"
                            )

        self.assertEqual(calls[-1][1], "session-end")
        self.assertEqual(stream.getvalue(), "")

    def test_project_dev_profile_overrides_only_its_explicit_fields(self) -> None:
        with mock.patch.object(
            self.backend,
            "_project_dev_profile_options",
            return_value={"opt-level": "1", "codegen-units": "32"},
        ):
            with mock.patch.dict(
                os.environ,
                {"SOLDR_PEP517_STABLE_TARGET_DIR": "0"},
                clear=True,
            ):
                env = self.backend._prep_env()
        self.assertNotIn("CARGO_PROFILE_DEV_OPT_LEVEL", env)
        self.assertNotIn("CARGO_PROFILE_DEV_CODEGEN_UNITS", env)
        self.assertEqual(env["CARGO_PROFILE_DEV_DEBUG"], "line-tables-only")
        self.assertEqual(env["CARGO_PROFILE_DEV_LTO"], "false")
        self.assertEqual(env["CARGO_PROFILE_DEV_INCREMENTAL"], "true")

    def test_project_identity_changes_for_build_configuration_changes(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "pyproject.toml").write_text("[tool.maturin]\nmodule-name='demo'\n")
            (root / "Cargo.toml").write_text("[package]\nname='demo'\n")
            with mock.patch.object(self.backend, "_project_root", return_value=root):
                first = self.backend._project_build_identity()
                (root / "Cargo.lock").write_text("version = 4\n")
                second = self.backend._project_build_identity()
        self.assertNotEqual(first, second)

    def test_stable_target_is_project_scoped_and_cargo_remains_authoritative(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as raw:
            with mock.patch.dict(
                os.environ,
                {
                    "SOLDR_PEP517_STABLE_TARGET_DIR": "1",
                    "USERPROFILE": raw,
                    "HOME": raw,
                },
                clear=True,
            ):
                env = self.backend._prep_env()
                expected_id = self.backend._project_build_identity()
                expected_target = self.backend._pep517_target_dir()
        self.assertEqual(env["SOLDR_PEP517_PROJECT_ID"], expected_id)
        self.assertEqual(env["CARGO_TARGET_DIR"], str(expected_target))
        self.assertEqual(Path(env["CARGO_TARGET_DIR"]).parent.name, "pep517")

    def test_stable_target_honors_effective_soldr_root(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            prod = root / ".soldr"
            dev = root / ".soldr-dev"
            custom = root / "custom"
            for selected in (prod, dev, custom):
                with self.subTest(selected=selected):
                    with mock.patch.dict(
                        os.environ,
                        {
                            "SOLDR_CACHE_DIR": str(selected),
                            "SOLDR_PEP517_STABLE_TARGET_DIR": "1",
                        },
                        clear=True,
                    ):
                        env = self.backend._prep_env()
                    target = Path(env["CARGO_TARGET_DIR"])
                    self.assertTrue(target.is_relative_to(selected))
                    self.assertEqual(target.parent.name, "pep517")
                    self.assertNotIn(".soldr", target.relative_to(selected).parts)

    def test_unset_root_follows_selected_official_or_dev_binary(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            for name in (".soldr", ".soldr-dev"):
                selected = base / name
                completed = subprocess.CompletedProcess(
                    ["soldr", "version", "--json"],
                    0,
                    json.dumps({"root_dir": str(selected)}),
                    "",
                )
                with self.subTest(name=name):
                    with mock.patch("subprocess.run", return_value=completed):
                        resolved = self.original_query_soldr_root({"PATH": name})
                    self.assertEqual(resolved, selected)
                    with mock.patch.object(
                        self.backend, "_query_soldr_root", return_value=resolved
                    ):
                        with mock.patch.dict(
                            os.environ,
                            {"SOLDR_PEP517_STABLE_TARGET_DIR": "1"},
                            clear=True,
                        ):
                            env = self.backend._prep_env()
                    self.assertEqual(Path(env["SOLDR_CACHE_DIR"]), selected)
                    self.assertTrue(
                        Path(env["CARGO_TARGET_DIR"]).is_relative_to(selected)
                    )

    def test_selected_soldr_root_is_probed_once_per_backend_process(self) -> None:
        selected = Path.cwd() / "selected-soldr"
        completed = subprocess.CompletedProcess(
            ["soldr", "version", "--json"],
            0,
            json.dumps({"root_dir": str(selected)}),
            "",
        )
        environment = {
            "PATH": os.environ.get("PATH", ""),
            "PATHEXT": os.environ.get("PATHEXT", ""),
            "HOME": "/same/home",
            "USERPROFILE": "C:/same/home",
        }
        with mock.patch("subprocess.run", return_value=completed) as run:
            first = self.original_query_soldr_root(environment)
            second = self.original_query_soldr_root(dict(environment))

        self.assertEqual(first, selected)
        self.assertEqual(second, selected)
        self.assertEqual(run.call_count, 1)

    def test_older_dev_binary_root_uses_status_compatibility_payload(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            selected = Path(raw) / ".soldr-dev"
            responses = [
                subprocess.CompletedProcess(
                    ["soldr", "version", "--json"],
                    0,
                    json.dumps({"soldr_version": "old"}),
                    "",
                ),
                subprocess.CompletedProcess(
                    ["soldr", "status", "--json"],
                    0,
                    json.dumps({"root_dir": str(selected)}),
                    "",
                ),
            ]
            with mock.patch("subprocess.run", side_effect=responses) as run:
                resolved = self.original_query_soldr_root({})
        self.assertEqual(resolved, selected)
        self.assertEqual(run.call_count, 2)

    def test_project_maturin_profile_is_not_overridden(self) -> None:
        with mock.patch.object(
            self.backend,
            "_project_maturin_options",
            return_value={"profile": "release", "editable-profile": "dev"},
        ):
            self.assertEqual(self.backend._profile_args(None), [])
            self.assertEqual(self.backend._profile_args(None, editable=True), [])

    def test_explicit_soldr_profile_overrides_project_profile(self) -> None:
        with mock.patch.dict(os.environ, {"SOLDR_PEP517_PROFILE": "ci"}, clear=False):
            with mock.patch.object(
                self.backend,
                "_project_maturin_options",
                return_value={"profile": "release"},
            ):
                self.assertEqual(self.backend._profile_args(None), ["--profile", "ci"])

    def test_explicit_target_config_has_stable_precedence(self) -> None:
        self.assertEqual(
            self.backend._target_args(
                {"target": "mac-arm64", "build-target": "win-x64"}
            ),
            ["--target", "mac-arm64"],
        )
        self.assertEqual(
            self.backend._target_args({"--target": ["win-x64", "win-arm64"]}),
            ["--target", "win-arm64"],
        )

    def test_caller_pyo3_environment_is_preserved(self) -> None:
        with mock.patch.dict(
            os.environ,
            {
                "PYO3_CROSS_LIB_DIR": "/caller/python",
                "PYO3_NO_PYTHON": "0",
                "SOLDR_PEP517_STABLE_TARGET_DIR": "0",
            },
            clear=False,
        ):
            env = self.backend._prep_env()
        self.assertEqual(env["PYO3_CROSS_LIB_DIR"], "/caller/python")
        self.assertEqual(env["PYO3_NO_PYTHON"], "0")

    def test_build_wheel_forwards_target_and_native_interpreter(self) -> None:
        calls = []
        with tempfile.TemporaryDirectory() as raw:
            wheel_dir = Path(raw)

            def fake_pep517(subcommand, *args, **kwargs):
                calls.append((subcommand, args, kwargs))
                (wheel_dir / "demo-0.1.0-py3-none-any.whl").write_bytes(b"wheel")

            with mock.patch.dict(
                os.environ, {"SOLDR_PEP517_WHEEL_CACHE": "off"}, clear=False
            ):
                with mock.patch.object(self.backend, "_maturin_pep517", fake_pep517):
                    produced = self.backend.build_wheel(
                        raw, {"target": "x86_64-pc-windows-msvc"}
                    )

        self.assertEqual(produced, "demo-0.1.0-py3-none-any.whl")
        subcommand, args, kwargs = calls[0]
        self.assertEqual(subcommand, "build-wheel")
        self.assertEqual(kwargs["build_label"], "wheel")
        self.assertEqual(
            kwargs["config_settings"], {"target": "x86_64-pc-windows-msvc"}
        )
        self.assertFalse(kwargs.get("editable", False))
        self.assertIn(sys.executable, args)
        target_index = args.index("--target")
        self.assertEqual(args[target_index + 1], "x86_64-pc-windows-msvc")

    def test_build_editable_threads_settings_into_target_preparation(self) -> None:
        calls = []
        settings = {"target": "linux-arm64", "editable-profile": "dev"}
        with tempfile.TemporaryDirectory() as raw:
            wheel_dir = Path(raw)

            def fake_pep517(subcommand, *args, **kwargs):
                calls.append((subcommand, args, kwargs))
                (wheel_dir / "demo-0.1.0-py3-none-any.whl").write_bytes(b"wheel")

            with mock.patch.dict(
                os.environ, {"SOLDR_PEP517_WHEEL_CACHE": "off"}, clear=False
            ):
                with mock.patch.object(self.backend, "_maturin_pep517", fake_pep517):
                    self.backend.build_editable(raw, settings)

        _, _, kwargs = calls[0]
        self.assertIs(kwargs["config_settings"], settings)
        self.assertTrue(kwargs["editable"])

    def test_wheel_cache_reuses_last_artifact_and_invalidates_on_source_change(
        self,
    ) -> None:
        calls = []
        stream = io.StringIO()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw) / "project"
            root.mkdir()
            (root / "pyproject.toml").write_text("[build-system]\nrequires=[]\n")
            (root / "Cargo.toml").write_text("[package]\nname='demo'\n")
            source = root / "native.bin"
            source.write_bytes(b"first")
            first = Path(raw) / "first"
            second = Path(raw) / "second"
            third = Path(raw) / "third"
            cache = Path(raw) / "cache"
            for directory in (first, second, third):
                directory.mkdir()

            def fake_pep517(subcommand, *args, **kwargs):
                calls.append((subcommand, args))
                out = Path(args[args.index("--out") + 1])
                (out / "demo-0.1.0-py3-none-any.whl").write_bytes(b"wheel")

            with mock.patch.object(self.backend, "_project_root", return_value=root):
                with mock.patch.dict(
                    os.environ,
                    {
                        "SOLDR_CACHE_DIR": str(cache),
                        "SOLDR_PEP517_STABLE_TARGET_DIR": "0",
                    },
                    clear=False,
                ):
                    with mock.patch.object(
                        self.backend, "_maturin_pep517", fake_pep517
                    ):
                        self.assertEqual(
                            self.backend.build_wheel(str(first)),
                            "demo-0.1.0-py3-none-any.whl",
                        )
                        with contextlib.redirect_stderr(stream):
                            self.assertEqual(
                                self.backend.build_wheel(str(second)),
                                "demo-0.1.0-py3-none-any.whl",
                            )
                        source.write_bytes(b"other")
                        modified = source.stat()
                        os.utime(
                            source,
                            ns=(
                                modified.st_atime_ns,
                                modified.st_mtime_ns + 1_000_000_000,
                            ),
                        )
                        self.assertEqual(
                            self.backend.build_wheel(str(third)),
                            "demo-0.1.0-py3-none-any.whl",
                        )

            self.assertEqual(len(calls), 2)
            self.assertEqual(
                (second / "demo-0.1.0-py3-none-any.whl").read_bytes(), b"wheel"
            )
            self.assertIn("wheel cache hit", stream.getvalue())
            self.assertEqual(len(list(cache.rglob("*.whl"))), 1)

    def test_wheel_cache_ignores_agent_worktrees_but_hashes_external_repos(
        self,
    ) -> None:
        calls = []
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw) / "project"
            root.mkdir()
            (root / "pyproject.toml").write_text("[build-system]\nrequires=[]\n")
            (root / "Cargo.toml").write_text(
                "[package]\nname='demo'\n[dependencies]\nexternal={path='.extern-repos/member'}\n"
            )
            runtime_source = root / ".claude" / "worktrees" / "stale" / "src.rs"
            runtime_source.parent.mkdir(parents=True)
            runtime_source.write_bytes(b"first")
            external_source = root / ".extern-repos" / "member" / "src" / "lib.rs"
            external_source.parent.mkdir(parents=True)
            external_source.write_bytes(b"first")
            first = Path(raw) / "first"
            second = Path(raw) / "second"
            third = Path(raw) / "third"
            cache = Path(raw) / "cache"
            first.mkdir()
            second.mkdir()
            third.mkdir()

            def fake_pep517(subcommand, *args, **kwargs):
                calls.append((subcommand, args))
                out = Path(args[args.index("--out") + 1])
                (out / "demo-0.1.0-py3-none-any.whl").write_bytes(b"wheel")

            with mock.patch.object(self.backend, "_project_root", return_value=root):
                with mock.patch.dict(
                    os.environ,
                    {
                        "SOLDR_CACHE_DIR": str(cache),
                        "SOLDR_PEP517_STABLE_TARGET_DIR": "0",
                    },
                    clear=False,
                ):
                    with mock.patch.object(
                        self.backend, "_maturin_pep517", fake_pep517
                    ):
                        self.backend.build_wheel(str(first))
                        runtime_source.write_bytes(b"second")
                        self.backend.build_wheel(str(second))
                        external_source.write_bytes(b"second")
                        self.backend.build_wheel(str(third))

        self.assertEqual(len(calls), 2)

    def test_wheel_cache_can_be_disabled(self) -> None:
        calls = []
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw) / "project"
            root.mkdir()
            (root / "pyproject.toml").write_text("[build-system]\nrequires=[]\n")
            (root / "Cargo.toml").write_text("[package]\nname='demo'\n")
            first = Path(raw) / "first"
            second = Path(raw) / "second"
            first.mkdir()
            second.mkdir()

            def fake_pep517(subcommand, *args, **kwargs):
                calls.append((subcommand, args))
                out = Path(args[args.index("--out") + 1])
                (out / "demo-0.1.0-py3-none-any.whl").write_bytes(b"wheel")

            with mock.patch.object(self.backend, "_project_root", return_value=root):
                with mock.patch.dict(
                    os.environ,
                    {"SOLDR_PEP517_WHEEL_CACHE": "off"},
                    clear=False,
                ):
                    with mock.patch.object(
                        self.backend, "_maturin_pep517", fake_pep517
                    ):
                        self.backend.build_wheel(str(first))
                        self.backend.build_wheel(str(second))

        self.assertEqual(len(calls), 2)

    def test_wheel_cache_ignores_generated_egg_info_metadata(self) -> None:
        calls = []
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw) / "project"
            root.mkdir()
            (root / "pyproject.toml").write_text("[build-system]\nrequires=[]\n")
            metadata = Path(raw) / "metadata"
            dist_info = metadata / "demo-0.1.0.dist-info"
            dist_info.mkdir(parents=True)
            (dist_info / "METADATA").write_text("Name: demo\nVersion: 0.1.0\n")
            egg_info = metadata / "demo.egg-info"
            egg_info.mkdir()
            sources = egg_info / "SOURCES.txt"
            sources.write_text("pyproject.toml\n")
            first = Path(raw) / "first"
            second = Path(raw) / "second"
            cache = Path(raw) / "cache"
            first.mkdir()
            second.mkdir()

            def fake_pep517(subcommand, *args, **kwargs):
                calls.append((subcommand, args))
                out = Path(args[args.index("--out") + 1])
                (out / "demo-0.1.0-py3-none-any.whl").write_bytes(b"wheel")

            with mock.patch.object(self.backend, "_project_root", return_value=root):
                with mock.patch.dict(
                    os.environ,
                    {
                        "SOLDR_CACHE_DIR": str(cache),
                        "SOLDR_PEP517_STABLE_TARGET_DIR": "0",
                    },
                    clear=False,
                ):
                    with mock.patch.object(
                        self.backend, "_maturin_pep517", fake_pep517
                    ):
                        self.backend.build_wheel(
                            str(first), metadata_directory=str(metadata)
                        )
                        sources.write_text("pyproject.toml\npython/demo.egg-info\n")
                        self.backend.build_wheel(
                            str(second), metadata_directory=str(metadata)
                        )

        self.assertEqual(len(calls), 1)

    def test_wheel_cache_reuses_delegate_artifacts(self) -> None:
        calls = []
        delegate: Any = types.ModuleType("pep517_cache_delegate")
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw) / "project"
            root.mkdir()
            (root / "pyproject.toml").write_text("[build-system]\nrequires=[]\n")
            (root / "Cargo.toml").write_text("[package]\nname='demo'\n")
            first = Path(raw) / "first"
            second = Path(raw) / "second"
            cache = Path(raw) / "cache"
            first.mkdir()
            second.mkdir()

            def build_wheel(wheel_directory, config_settings, metadata_directory):
                calls.append((wheel_directory, config_settings, metadata_directory))
                # Setuptools creates these under the project while preparing a
                # wheel. They must not invalidate the post-build cache entry.
                generated = root / "build" / "lib"
                generated.mkdir(parents=True)
                (generated / "generated.py").write_text("generated\n")
                egg_info = root / "demo.egg-info"
                egg_info.mkdir()
                (egg_info / "PKG-INFO").write_text("Metadata-Version: 2.1\n")
                (Path(wheel_directory) / "demo-0.1.0-py3-none-any.whl").write_bytes(
                    b"wheel"
                )
                return "demo-0.1.0-py3-none-any.whl"

            delegate.build_wheel = build_wheel
            with mock.patch.dict(sys.modules, {"pep517_cache_delegate": delegate}):
                with mock.patch.object(
                    self.backend, "_project_root", return_value=root
                ):
                    with mock.patch.object(
                        self.backend,
                        "_project_soldr_options",
                        return_value={"delegate-backend": "pep517_cache_delegate"},
                    ):
                        with mock.patch.dict(
                            os.environ,
                            {
                                "SOLDR_CACHE_DIR": str(cache),
                                "SOLDR_PEP517_STABLE_TARGET_DIR": "0",
                            },
                            clear=False,
                        ):
                            self.backend.build_wheel(str(first))
                            self.backend.build_wheel(str(second))

            self.assertEqual(len(calls), 1)
            self.assertTrue((second / "demo-0.1.0-py3-none-any.whl").is_file())

    def test_delegate_backend_receives_hooks_under_managed_environment(self) -> None:
        observed = {}
        delegate: Any = types.ModuleType("pep517_delegate_test")

        def get_requires(config_settings):
            observed["requires"] = config_settings
            observed["requires_wrapper"] = os.environ.get("RUSTC_WRAPPER")
            return ["setuptools"]

        def build_wheel(wheel_directory, config_settings, metadata_directory):
            observed["wheel"] = (wheel_directory, config_settings, metadata_directory)
            observed["wheel_wrapper"] = os.environ.get("RUSTC_WRAPPER")
            observed["wheel_target"] = os.environ.get("CARGO_TARGET_DIR")
            observed["wheel_profile"] = os.environ.get("SOLDR_PEP517_PROFILE")
            return "demo-0.1.0-py3-none-any.whl"

        def build_editable(wheel_directory, config_settings, metadata_directory):
            observed["editable"] = config_settings
            return "demo-0.1.0-py3-none-any.whl"

        def prepare_metadata(metadata_directory, config_settings):
            observed["metadata"] = config_settings
            return "demo-0.1.0.dist-info"

        delegate.get_requires_for_build_wheel = get_requires
        delegate.build_wheel = build_wheel
        delegate.build_editable = build_editable
        delegate.prepare_metadata_for_build_wheel = prepare_metadata

        with mock.patch.dict(sys.modules, {"pep517_delegate_test": delegate}):
            with mock.patch.object(
                self.backend,
                "_project_soldr_options",
                return_value={"delegate-backend": "pep517_delegate_test"},
            ):
                with mock.patch.dict(
                    os.environ, {"RUSTC_WRAPPER": "caller"}, clear=False
                ):
                    self.assertEqual(
                        self.backend.get_requires_for_build_wheel({"profile": "dev"}),
                        ["setuptools"],
                    )
                    self.assertEqual(
                        self.backend.build_wheel("wheel", {"profile": "dev"}, "meta"),
                        "demo-0.1.0-py3-none-any.whl",
                    )
                    self.assertEqual(
                        self.backend.build_editable("wheel", {"editable": "1"}, None),
                        "demo-0.1.0-py3-none-any.whl",
                    )
                    self.assertEqual(
                        self.backend.prepare_metadata_for_build_wheel("meta", None),
                        "demo-0.1.0.dist-info",
                    )
                    self.assertEqual(os.environ["RUSTC_WRAPPER"], "caller")

        self.assertEqual(observed["requires_wrapper"], "caller")
        self.assertEqual(observed["wheel_wrapper"], "caller")
        self.assertIsNotNone(observed["wheel_target"])
        self.assertEqual(observed["wheel_profile"], "dev")
        self.assertEqual(observed["requires"], {"profile": "dev"})
        self.assertEqual(observed["editable"], {"editable": "1"})

    def test_wheel_build_disables_staged_artifacts_by_default(self) -> None:
        """soldr#1867: staged-artifact reuse is off unless the caller asks.

        A wheel build is cold and one-shot, so reuse buys little — but a
        building soldr that predates zccache b81b8131 can serve a stale
        generation for a key it has already proven non-deterministic. That
        surfaces as `could not compile <trivial crate>`, naming a different
        crate each run, with nothing pointing at the cache.

        Asserted against `_prep_env`, the pure env builder, rather than by
        driving `build_wheel`: the latter acquires a build lease and leaves
        state that perturbs the idle-watchdog tests later in this file.
        """
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("ZCCACHE_STAGED_ARTIFACTS", None)
            env = self.backend._prep_env({"profile": "dev"})

        self.assertEqual(env.get("ZCCACHE_STAGED_ARTIFACTS"), "off")

    def test_caller_can_re_enable_staged_artifacts(self) -> None:
        """The default must not override an explicit choice (soldr#1867)."""
        with mock.patch.dict(
            os.environ, {"ZCCACHE_STAGED_ARTIFACTS": "on"}, clear=False
        ):
            env = self.backend._prep_env({"profile": "dev"})

        self.assertEqual(env.get("ZCCACHE_STAGED_ARTIFACTS"), "on")

    def test_delegate_hook_holds_build_lease_for_full_call(self) -> None:
        events = []
        delegate: Any = types.ModuleType("pep517_leased_delegate")

        def build_wheel(*_args, **_kwargs):
            events.append("hook")
            return "demo.whl"

        @contextlib.contextmanager
        def recording_lease(environment):
            events.append(("lease-enter", environment["SOLDR_CACHE_DIR"]))
            yield
            events.append("lease-exit")

        delegate.build_wheel = build_wheel
        with mock.patch.dict(sys.modules, {"pep517_leased_delegate": delegate}):
            with mock.patch.object(
                self.backend,
                "_project_soldr_options",
                return_value={"delegate-backend": "pep517_leased_delegate"},
            ):
                with mock.patch.object(
                    self.backend, "_hold_build_lease", recording_lease
                ):
                    result = self.backend._delegate_hook(
                        "build_wheel", "wheel", None, None
                    )
        self.assertEqual(result, "demo.whl")
        self.assertEqual(events[0][0], "lease-enter")
        self.assertEqual(events[1:], ["hook", "lease-exit"])

    def test_build_lease_helper_uses_pipe_lifetime(self) -> None:
        class FakeProcess:
            def __init__(self):
                self.stdin = io.BytesIO()
                self.stdout = io.BytesIO(b"ready\n")
                self.stderr = io.BytesIO()
                self.wait_timeouts = []
                self.killed = False

            def wait(self, timeout=None):
                self.wait_timeouts.append(timeout)
                return 0

            def kill(self):
                self.killed = True

        process = FakeProcess()
        environment = {"SOLDR_CACHE_DIR": "/selected/root"}
        with mock.patch.object(
            self.backend.subprocess, "Popen", return_value=process
        ) as popen:
            with self.original_hold_build_lease(environment):
                self.assertFalse(process.stdin.closed)
        self.assertTrue(process.stdin.closed)
        self.assertEqual(process.wait_timeouts, [10])
        self.assertFalse(process.killed)
        args, kwargs = popen.call_args
        self.assertEqual(args[0], ["soldr", "gc", "hold-build-lease"])
        self.assertEqual(kwargs["env"], environment)

    def test_delegate_profile_setting_overrides_environment_temporarily(self) -> None:
        observed = {}
        delegate: Any = types.ModuleType("pep517_profile_delegate_test")

        def build_wheel(wheel_directory, config_settings, metadata_directory):
            observed["wheel_profile"] = os.environ.get("SOLDR_PEP517_PROFILE")
            return "demo-0.1.0-py3-none-any.whl"

        def build_editable(wheel_directory, config_settings, metadata_directory):
            observed["editable_profile"] = os.environ.get("SOLDR_PEP517_PROFILE")
            return "demo-0.1.0-py3-none-any.whl"

        delegate.build_wheel = build_wheel
        delegate.build_editable = build_editable
        with mock.patch.dict(sys.modules, {"pep517_profile_delegate_test": delegate}):
            with mock.patch.object(
                self.backend,
                "_project_soldr_options",
                return_value={"delegate-backend": "pep517_profile_delegate_test"},
            ):
                with mock.patch.dict(
                    os.environ,
                    {"SOLDR_PEP517_PROFILE": "caller"},
                    clear=False,
                ):
                    self.backend.build_wheel("wheel", {"profile": "release"}, None)
                    self.backend.build_editable(
                        "wheel", {"editable-profile": "dev"}, None
                    )
                    self.assertEqual(os.environ["SOLDR_PEP517_PROFILE"], "caller")

        self.assertEqual(observed["wheel_profile"], "release")
        self.assertEqual(observed["editable_profile"], "dev")

    def test_delegate_backend_rejects_recursive_soldr(self) -> None:
        with mock.patch.object(
            self.backend,
            "_project_soldr_options",
            return_value={"delegate-backend": "soldr"},
        ):
            with self.assertRaisesRegex(RuntimeError, "cannot delegate back to soldr"):
                self.backend.get_requires_for_build_wheel()


class Pep517IdleWatchdogTest(unittest.TestCase):
    """soldr#1803 — the maturin child is killed only on sustained silence,
    never while it is still producing output."""

    def setUp(self) -> None:
        self.backend = _load_backend()

    def test_idle_timeout_parsing(self) -> None:
        resolve = self.backend._pep517_idle_timeout
        default = self.backend._PEP517_IDLE_TIMEOUT_DEFAULT
        env_var = self.backend._PEP517_IDLE_TIMEOUT_ENV
        self.assertEqual(resolve({}), default)
        self.assertEqual(resolve({env_var: "90"}), 90.0)
        self.assertIsNone(resolve({env_var: "0"}))
        self.assertIsNone(resolve({env_var: "-5"}))
        self.assertEqual(resolve({env_var: "not-a-number"}), default)

    def _child_env(self, idle_secs: str) -> "dict[str, str]":
        env = dict(os.environ)
        env[self.backend._PEP517_IDLE_TIMEOUT_ENV] = idle_secs
        # These exercise the relay mechanics (decoding, watchdog, child env),
        # not the presentation layer, and assert on exact relayed bytes. Pin
        # per-line timestamping off so they stay deterministic regardless of
        # the runner's env; the stamping behavior has its own coverage in
        # tests/test_pep517_timestamps.py (soldr#1802).
        env[self.backend._TIMESTAMP_LINES_ENV_VAR] = "0"
        return env

    def test_chatty_child_outlives_idle_window(self) -> None:
        # Prints every 0.2s for ~1.6s against a 0.75s idle limit: total
        # runtime far exceeds the limit but every chunk resets the deadline,
        # so the child must complete successfully.
        code = "import time\nfor _ in range(8):\n    print('tick', flush=True)\n    time.sleep(0.2)\n"
        self.backend._run_pep517_streaming(
            [sys.executable, "-u", "-c", code], env=self._child_env("0.75")
        )

    def test_silent_child_is_killed(self) -> None:
        code = "import time\ntime.sleep(30)\n"
        with self.assertRaises(subprocess.TimeoutExpired):
            self.backend._run_pep517_streaming(
                [sys.executable, "-u", "-c", code], env=self._child_env("0.75")
            )

    def test_nonzero_exit_raises_called_process_error(self) -> None:
        with self.assertRaises(subprocess.CalledProcessError):
            self.backend._run_pep517_streaming(
                [sys.executable, "-u", "-c", "raise SystemExit(3)"],
                env=self._child_env("5"),
            )

    def test_nonzero_exit_repeats_a_concise_diagnostic_summary(self) -> None:
        stderr = io.StringIO()
        rendered = (
            "error[E0277]: `Handle` cannot be sent safely\n"
            "  --> src/main.rs:12:5\n"
            "note: required by `thread::spawn`\n"
        )
        cargo_message = json.dumps(
            {
                "reason": "compiler-message",
                "message": {"rendered": rendered},
            }
        )
        code = (
            "import sys\n"
            "for step in range(100):\n"
            "    print(f'Building [====> ] {step}/100: dependency\u2026', file=sys.stderr)\n"
            f"print({cargo_message!r})\n"
            "raise SystemExit(101)\n"
        )
        with tempfile.TemporaryDirectory() as root:
            env = self._child_env("5")
            env["SOLDR_CACHE_DIR"] = root
            with contextlib.redirect_stderr(stderr):
                with self.assertRaises(subprocess.CalledProcessError):
                    self.backend._run_pep517_streaming(
                        [sys.executable, "-u", "-c", code], env=env
                    )

            summary = stderr.getvalue().split("soldr: PEP 517 build failed", 1)[1]
            self.assertIn("exit code 101", summary)
            self.assertIn("error[E0277]", summary)
            self.assertIn("src/main.rs:12:5", summary)
            self.assertNotIn("99/100", summary)

            log_prefix = "soldr: full PEP 517 build log: "
            log_line = next(
                line for line in summary.splitlines() if line.startswith(log_prefix)
            )
            log_path = Path(log_line.removeprefix(log_prefix))
            self.assertTrue(log_path.is_file())
            full_log = log_path.read_text(encoding="utf-8")
            self.assertIn("99/100", full_log)
            self.assertIn(cargo_message, full_log)

    def test_failure_log_drains_slow_text_sink_before_returning(self) -> None:
        class SlowSink(io.StringIO):
            def write(self, value: str) -> int:
                time.sleep(0.3)
                return super().write(value)

        stderr = SlowSink()
        code = (
            "import sys, time\n"
            "print('first diagnostic', file=sys.stderr, flush=True)\n"
            "time.sleep(0.2)\n"
            "print('error: final diagnostic', file=sys.stderr, flush=True)\n"
            "raise SystemExit(2)\n"
        )
        with tempfile.TemporaryDirectory() as root:
            env = self._child_env("5")
            env["SOLDR_CACHE_DIR"] = root
            with contextlib.redirect_stderr(stderr):
                with self.assertRaises(subprocess.CalledProcessError):
                    self.backend._run_pep517_streaming(
                        [sys.executable, "-u", "-c", code], env=env
                    )

            summary = stderr.getvalue().split("soldr: PEP 517 build failed", 1)[1]
            self.assertIn("full PEP 517 build log:", summary)
            log_prefix = "soldr: full PEP 517 build log: "
            log_line = next(
                line for line in summary.splitlines() if line.startswith(log_prefix)
            )
            full_log = Path(log_line.removeprefix(log_prefix)).read_text(
                encoding="utf-8"
            )
            self.assertIn("first diagnostic", full_log)
            self.assertIn("error: final diagnostic", full_log)

    def test_relay_failure_kills_child_and_retains_partial_log(self) -> None:
        class BrokenSink(io.StringIO):
            def write(self, _value: str) -> int:
                raise BrokenPipeError("capture pipe closed")

        code = (
            "import sys, time\n"
            "print('diagnostic before broken pipe', file=sys.stderr, flush=True)\n"
            "time.sleep(30)\n"
        )
        with tempfile.TemporaryDirectory() as root:
            env = self._child_env("5")
            env["SOLDR_CACHE_DIR"] = root
            started = time.monotonic()
            with contextlib.redirect_stderr(BrokenSink()):
                with self.assertRaisesRegex(
                    RuntimeError, "output relay failed"
                ) as raised:
                    self.backend._run_pep517_streaming(
                        [sys.executable, "-u", "-c", code], env=env
                    )
            self.assertLess(time.monotonic() - started, 3)

            log_prefix = "possibly incomplete PEP 517 build log: "
            message = str(raised.exception)
            self.assertIn(log_prefix, message)
            log_path = Path(message.split(log_prefix, 1)[1])
            self.assertTrue(log_path.is_file())
            self.assertIn(
                "diagnostic before broken pipe",
                log_path.read_text(encoding="utf-8"),
            )

    def test_utf8_child_output_is_decoded_before_text_relay(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()
        code = (
            "import sys\n"
            "sys.stdout.buffer.write('Building crate\u2026\\n'.encode())\n"
            "sys.stdout.buffer.flush()\n"
            "sys.stderr.buffer.write('\U0001f4a5 error[E0277]: not Send\\n'.encode())\n"
            "sys.stderr.buffer.flush()\n"
        )
        with tempfile.TemporaryDirectory() as root:
            env = self._child_env("5")
            env["SOLDR_CACHE_DIR"] = root
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                self.backend._run_pep517_streaming(
                    [sys.executable, "-u", "-c", code], env=env
                )
            self.assertEqual(list(Path(root).rglob("*.log")), [])

        self.assertEqual(stdout.getvalue(), "Building crate\u2026\n")
        self.assertEqual(stderr.getvalue(), "\U0001f4a5 error[E0277]: not Send\n")
        self.assertNotIn("\u00e2\u20ac\u00a6", stdout.getvalue())
        self.assertNotIn("\u00f0\u0178", stderr.getvalue())

    def test_disables_cargo_progress_redraws_for_piped_build(self) -> None:
        # Cargo's TTY-style progress stream becomes hundreds of near-identical
        # lines after pip captures it. Normal "Compiling ..." events still
        # provide liveness; the 30-minute idle watchdog covers a silent link.
        marker = Path(tempfile.mkdtemp()) / "progress-env.json"
        code = (
            "import json, os, pathlib, sys\n"
            f"pathlib.Path({str(marker)!r}).write_text(json.dumps(\n"
            "    {k: os.environ.get(k) for k in\n"
            "     ('CARGO_TERM_PROGRESS_WHEN', 'CARGO_TERM_COLOR', 'NO_COLOR')}))\n"
            "print('done', flush=True)\n"
        )
        self.backend._run_pep517_streaming(
            [sys.executable, "-u", "-c", code], env=self._child_env("5")
        )
        emitted = json.loads(marker.read_text())
        self.assertEqual(emitted["CARGO_TERM_PROGRESS_WHEN"], "never")
        self.assertEqual(emitted["CARGO_TERM_COLOR"], "never")
        self.assertEqual(emitted["NO_COLOR"], "1")


if __name__ == "__main__":
    unittest.main()
