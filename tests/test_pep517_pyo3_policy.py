from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock


def _load_backend():
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
        self.assertEqual(self.backend._profile_args(None), ["--profile", "dev"])
        self.assertEqual(env["SOLDR_PEP517_LINKER"], "auto")

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
                    self.backend.subprocess,
                    "check_call",
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
                with mock.patch.object(self.backend.subprocess, "check_call"):
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
                    self.backend.subprocess,
                    "check_call",
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

            with mock.patch.object(self.backend, "_maturin_pep517", fake_pep517):
                produced = self.backend.build_wheel(
                    raw, {"target": "x86_64-pc-windows-msvc"}
                )

        self.assertEqual(produced, "demo-0.1.0-py3-none-any.whl")
        subcommand, args, kwargs = calls[0]
        self.assertEqual(subcommand, "build-wheel")
        self.assertEqual(kwargs["build_label"], "wheel")
        self.assertIn(sys.executable, args)
        target_index = args.index("--target")
        self.assertEqual(args[target_index + 1], "x86_64-pc-windows-msvc")

    def test_delegate_backend_receives_hooks_under_managed_environment(self) -> None:
        observed = {}
        delegate = types.ModuleType("pep517_delegate_test")

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

    def test_delegate_profile_setting_overrides_environment_temporarily(self) -> None:
        observed = {}
        delegate = types.ModuleType("pep517_profile_delegate_test")

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


if __name__ == "__main__":
    unittest.main()
