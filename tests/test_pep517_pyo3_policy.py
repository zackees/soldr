from __future__ import annotations

import importlib.util
import os
import sys
import tempfile
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
            with mock.patch.dict(os.environ, {"SOLDR_PEP517_STABLE_TARGET_DIR": "0"}, clear=False):
                env = self.backend._prep_env()
        finally:
            if previous is not None:
                os.environ["PYO3_NO_PYTHON"] = previous
        self.assertNotIn("PYO3_NO_PYTHON", env)

    def test_local_profile_defaults_to_lightweight_incremental_dev(self) -> None:
        with mock.patch.dict(
            os.environ,
            {"SOLDR_PEP517_STABLE_TARGET_DIR": "0"},
            clear=False,
        ):
            env = self.backend._prep_env()
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
                "CARGO_PROFILE_DEV_DEBUG": "2",
                "SOLDR_PEP517_STABLE_TARGET_DIR": "0",
            },
            clear=False,
        ):
            env = self.backend._prep_env()
        self.assertEqual(env["CARGO_PROFILE_DEV_DEBUG"], "2")
        self.assertEqual(self.backend._profile_args({"profile": "ci"}), ["--profile", "ci"])

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
            self.backend._target_args({"target": "mac-arm64", "build-target": "win-x64"}),
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

            def fake_pep517(subcommand, *args):
                calls.append((subcommand, args))
                (wheel_dir / "demo-0.1.0-py3-none-any.whl").write_bytes(b"wheel")

            with mock.patch.object(self.backend, "_maturin_pep517", fake_pep517):
                produced = self.backend.build_wheel(raw, {"target": "x86_64-pc-windows-msvc"})

        self.assertEqual(produced, "demo-0.1.0-py3-none-any.whl")
        subcommand, args = calls[0]
        self.assertEqual(subcommand, "build-wheel")
        self.assertIn(sys.executable, args)
        target_index = args.index("--target")
        self.assertEqual(args[target_index + 1], "x86_64-pc-windows-msvc")


if __name__ == "__main__":
    unittest.main()
