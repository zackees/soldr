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
