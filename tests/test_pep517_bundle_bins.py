"""soldr#3239: `[tool.soldr.pep517] bundle-bins` stages Cargo bins into the wheel."""

from __future__ import annotations

import base64
import contextlib
import csv
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import types
import unittest
import zipfile
from pathlib import Path
from typing import Any
from unittest import mock

from conftest import load_script_module

SRC = Path(__file__).parents[1] / "src" / "soldr"
WHEEL_NAME = "demo-0.1.0-cp310-abi3-linux_x86_64.whl"
PYPROJECT = """\
[build-system]
requires = ["soldr"]
build-backend = "soldr"

[project]
name = "demo"
# a comment with # and [brackets] = "noise"

[tool.soldr.pep517]
bundle-bins = [
  { bin = "demo-cli", package = "demo" },  # installed onto PATH
  { bin = "helper", dest = 'platlib/demo/_bin' },
]

[tool.maturin]
module-name = "demo._native"
"""


def _helper() -> Any:
    return load_script_module(SRC / "_bundle_bins.py", "soldr_test_bundle_bins")


def _record_hash(data: bytes) -> str:
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
    return f"sha256={digest.decode()}"


def _write_wheel(path: Path, *, purelib: bool = False) -> None:
    files = {
        "demo/__init__.py": b"from ._native import *\n",
        "demo/_native.abi3.so": b"\x7fELF extension",
        "demo-0.1.0.dist-info/METADATA": b"Metadata-Version: 2.1\nName: demo\n",
        "demo-0.1.0.dist-info/WHEEL": (
            "Wheel-Version: 1.0\nRoot-Is-Purelib: "
            + ("true" if purelib else "false")
            + "\nTag: cp310-abi3-linux_x86_64\n"
        ).encode(),
    }
    with zipfile.ZipFile(path, "w") as wheel:
        rows = []
        for name, data in files.items():
            wheel.writestr(name, data)
            rows.append(f"{name},{_record_hash(data)},{len(data)}")
        rows.append("demo-0.1.0.dist-info/RECORD,,")
        wheel.writestr("demo-0.1.0.dist-info/RECORD", "\n".join(rows) + "\n")


class ConfigTest(unittest.TestCase):
    def setUp(self) -> None:
        self.helper = _helper()

    def _read(self, text: str) -> list[Any]:
        with tempfile.TemporaryDirectory() as raw:
            pyproject = Path(raw) / "pyproject.toml"
            pyproject.write_text(text, encoding="utf-8")
            return self.helper.read_bundle_bins(pyproject)

    def test_reads_entries_with_default_dest(self) -> None:
        self.assertEqual(
            self._read(PYPROJECT),
            [
                self.helper.BundleBin(bin="demo-cli", package="demo", dest="scripts"),
                self.helper.BundleBin(bin="helper", dest="platlib/demo/_bin"),
            ],
        )

    def test_python310_fallback_parser_matches_tomllib(self) -> None:
        """3.10 has no tomllib; the narrow parser must agree on the documented shape."""
        self.assertEqual(
            self.helper._fallback_entries(PYPROJECT),
            [
                {"bin": "demo-cli", "package": "demo"},
                {"bin": "helper", "dest": "platlib/demo/_bin"},
            ],
        )
        with mock.patch.dict(sys.modules, {"tomllib": None}):
            fallback = self._read(PYPROJECT)
        self.assertEqual(fallback, self._read(PYPROJECT))

    def test_fallback_parser_accepts_single_line_array(self) -> None:
        text = '[tool.soldr.pep517]\nbundle-bins = [{ bin = "x" }]\n[other]\nbundle-bins = [{ bin = "y" }]\n'
        self.assertEqual(self.helper._fallback_entries(text), [{"bin": "x"}])

    def test_absent_configuration_is_empty(self) -> None:
        self.assertEqual(
            self._read('[tool.soldr.pep517]\ndelegate-backend = "x"\n'), []
        )
        with tempfile.TemporaryDirectory() as raw:
            self.assertEqual(
                self.helper.read_bundle_bins(Path(raw) / "missing.toml"), []
            )

    def test_invalid_entries_are_refused_by_name(self) -> None:
        cases = {
            'bundle-bins = "demo"': "array of tables",
            'bundle-bins = ["demo"]': "must be a table",
            'bundle-bins = [{ package = "demo" }]': "needs `bin`",
            'bundle-bins = [{ bin = "demo", target = "x" }]': "unknown key(s) target",
            'bundle-bins = [{ bin = "demo", dest = "bin" }]': "<scheme>",
            'bundle-bins = [{ bin = "demo", dest = "scripts/../x" }]': "<scheme>",
            'bundle-bins = [{ bin = "demo", dest = "platlib//x" }]': "<scheme>",
            'bundle-bins = [{ bin = "demo", dest = "platlib\\\\x" }]': "<scheme>",
            'bundle-bins = [{ bin = "../demo" }]': "needs `bin`",
        }
        for body, message in cases.items():
            with self.subTest(body=body):
                with self.assertRaises(self.helper.BundleBinsError) as raised:
                    self._read(f"[tool.soldr.pep517]\n{body}\n")
                self.assertIn(message, str(raised.exception))


class BuildTest(unittest.TestCase):
    def setUp(self) -> None:
        self.helper = _helper()

    def test_command_uses_blessed_build_surface(self) -> None:
        entry = self.helper.BundleBin(bin="demo-cli", package="demo")
        self.assertEqual(
            self.helper.cargo_build_command(
                entry,
                manifest_path=Path("crates/demo/Cargo.toml"),
                profile_args=["--profile", "dev"],
                target_args=["--target", "aarch64-apple-darwin"],
            ),
            [
                "soldr",
                "build",
                "--bin",
                "demo-cli",
                "--message-format=json-render-diagnostics",
                "--package",
                "demo",
                "--manifest-path",
                str(Path("crates/demo/Cargo.toml")),
                "--profile",
                "dev",
                "--target",
                "aarch64-apple-darwin",
            ],
        )

    def test_executable_is_taken_from_the_matching_bin_artifact(self) -> None:
        stdout = "\n".join(
            [
                "warning: noise",
                '{"reason":"compiler-artifact","target":{"name":"demo-cli","kind":["lib"]},"executable":null}',
                '{"reason":"compiler-artifact","target":{"name":"other","kind":["bin"]},"executable":"/t/other"}',
                '{"reason":"compiler-artifact","target":{"name":"demo-cli","kind":["bin"]},"executable":"/t/demo-cli"}',
                '{"reason":"build-finished","success":true}',
            ]
        )
        self.assertEqual(
            self.helper.executable_from_messages(stdout, "demo-cli"),
            Path("/t/demo-cli"),
        )
        with self.assertRaises(self.helper.BundleBinsError):
            self.helper.executable_from_messages(stdout, "missing")

    def test_failed_build_names_the_bin(self) -> None:
        entry = self.helper.BundleBin(bin="demo-cli")

        def run(*_args: Any, **_kwargs: Any) -> Any:
            return subprocess.CompletedProcess([], 101, stdout="")

        with self.assertRaises(self.helper.BundleBinsError) as raised:
            self.helper.build_bundle_bin(entry, ["soldr", "build"], {}, run=run)
        self.assertIn("exited with 101", str(raised.exception))
        self.assertIn("demo-cli", str(raised.exception))

    def test_successful_build_returns_the_reported_file(self) -> None:
        entry = self.helper.BundleBin(bin="demo-cli")
        with tempfile.TemporaryDirectory() as raw:
            executable = Path(raw) / "demo-cli"
            executable.write_bytes(b"bin")
            message = json.dumps(
                {
                    "reason": "compiler-artifact",
                    "target": {"name": "demo-cli", "kind": ["bin"]},
                    "executable": str(executable),
                }
            )

            def run(command: Any, **kwargs: Any) -> Any:
                self.assertEqual(kwargs["env"], {"K": "V"})
                self.assertEqual(kwargs["stdout"], subprocess.PIPE)
                return subprocess.CompletedProcess(command, 0, stdout=message)

            self.assertEqual(
                self.helper.build_bundle_bin(entry, ["soldr"], {"K": "V"}, run=run),
                executable,
            )


class WheelTest(unittest.TestCase):
    def setUp(self) -> None:
        self.helper = _helper()

    def test_arcname_follows_wheel_install_schemes(self) -> None:
        arcname = self.helper.wheel_arcname
        self.assertEqual(
            arcname("demo-0.1.0", "scripts", "x", False), "demo-0.1.0.data/scripts/x"
        )
        self.assertEqual(
            arcname("demo-0.1.0", "platlib/demo/_bin", "x", False), "demo/_bin/x"
        )
        self.assertEqual(
            arcname("demo-0.1.0", "platlib/demo", "x", True),
            "demo-0.1.0.data/platlib/demo/x",
        )
        self.assertEqual(arcname("demo-0.1.0", "purelib", "x", True), "x")
        self.assertEqual(
            arcname("demo-0.1.0", "data/share", "x", False),
            "demo-0.1.0.data/data/share/x",
        )

    def test_added_bins_are_executable_and_recorded(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            wheel = Path(raw) / WHEEL_NAME
            _write_wheel(wheel)
            cli = Path(raw) / "demo-cli"
            cli.write_bytes(b"\x7fELF cli")
            helper_bin = Path(raw) / "helper.exe"
            helper_bin.write_bytes(b"MZ helper")
            added = self.helper.add_files_to_wheel(
                wheel,
                [
                    (self.helper.BundleBin(bin="demo-cli"), cli),
                    (
                        self.helper.BundleBin(bin="helper", dest="platlib/demo/_bin"),
                        helper_bin,
                    ),
                ],
            )
            self.assertEqual(
                added, ["demo-0.1.0.data/scripts/demo-cli", "demo/_bin/helper.exe"]
            )
            with zipfile.ZipFile(wheel) as archive:
                self.assertIsNone(archive.testzip())
                names = archive.namelist()
                self.assertEqual(names[-1], "demo-0.1.0.dist-info/RECORD")
                for name in added:
                    self.assertEqual(
                        (archive.getinfo(name).external_attr >> 16) & 0o777, 0o755
                    )
                self.assertEqual(archive.read("demo/_bin/helper.exe"), b"MZ helper")
                rows = list(csv.reader(io.StringIO(archive.read(names[-1]).decode())))
                recorded = {row[0]: row for row in rows}
                self.assertEqual(set(recorded), set(names))
                self.assertEqual(recorded["demo-0.1.0.dist-info/RECORD"][1:], ["", ""])
                for name in names[:-1]:
                    data = archive.read(name)
                    self.assertEqual(
                        recorded[name][1:], [_record_hash(data), str(len(data))]
                    )
            self.assertEqual(
                sorted(p.name for p in Path(raw).iterdir()),
                sorted([WHEEL_NAME, "demo-cli", "helper.exe"]),
            )

    def test_collision_is_refused_without_touching_the_wheel(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            wheel = Path(raw) / WHEEL_NAME
            _write_wheel(wheel)
            before = wheel.read_bytes()
            clash = Path(raw) / "__init__.py"
            clash.write_bytes(b"x")
            with self.assertRaises(self.helper.BundleBinsError):
                self.helper.add_files_to_wheel(
                    wheel,
                    [(self.helper.BundleBin(bin="x", dest="platlib/demo"), clash)],
                )
            self.assertEqual(wheel.read_bytes(), before)


class BackendIntegrationTest(unittest.TestCase):
    """The native maturin hooks build and stage configured bins."""

    def setUp(self) -> None:
        self.backend = load_script_module(
            SRC / "__init__.py", "soldr_bundle_bins_backend"
        )
        self.helper = self.backend._bundle_bins_module()
        self.scratch = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.scratch, ignore_errors=True)
        self.root = self.scratch / "project"
        self.root.mkdir()
        (self.root / "pyproject.toml").write_text(PYPROJECT, encoding="utf-8")
        (self.root / "Cargo.toml").write_text(
            "[package]\nname = 'demo'\n", encoding="utf-8"
        )
        self.out = self.scratch / "out"
        self.out.mkdir()

        @contextlib.contextmanager
        def no_op_build_lease(_environment: Any) -> Any:
            yield

        for patcher in (
            mock.patch.object(self.backend, "_project_root", return_value=self.root),
            mock.patch.object(self.backend, "_hold_build_lease", no_op_build_lease),
            mock.patch.dict(
                os.environ,
                {
                    "SOLDR_CACHE_DIR": str(self.scratch / "cache"),
                    "SOLDR_PEP517_STABLE_TARGET_DIR": "0",
                    "SOLDR_PEP517_WHEEL_CACHE": "off",
                    "SOLDR_PEP517_STATS": "off",
                },
            ),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)
        for name in ("SOLDR_PEP517_PROFILE", "PYO3_PYTHON"):
            env_patch = mock.patch.dict(os.environ)
            env_patch.start()
            self.addCleanup(env_patch.stop)
            os.environ.pop(name, None)

    def _build(
        self, hook: str, config_settings: Any = None
    ) -> list[tuple[Any, list[str], dict[str, str]]]:
        calls: list[tuple[Any, list[str], dict[str, str]]] = []

        def maturin(subcommand: str, *args: str, **_kwargs: Any) -> None:
            self.assertEqual(subcommand, "build-wheel")
            _write_wheel(self.out / WHEEL_NAME)

        def build_bin(entry: Any, command: list[str], env: dict[str, str]) -> Path:
            calls.append((entry, command, dict(env)))
            path = self.scratch / entry.bin
            path.write_bytes(entry.bin.encode())
            return path

        with mock.patch.object(self.backend, "_maturin_pep517", maturin):
            with mock.patch.object(self.helper, "build_bundle_bin", build_bin):
                result = getattr(self.backend, hook)(str(self.out), config_settings)
        self.assertEqual(result, WHEEL_NAME)
        return calls

    def test_wheel_bundles_each_bin_with_maturins_profile_and_interpreter(self) -> None:
        calls = self._build("build_wheel", {"--target": "x86_64-unknown-linux-gnu"})
        self.assertEqual([entry.bin for entry, _, _ in calls], ["demo-cli", "helper"])
        _, command, env = calls[0]
        self.assertEqual(command[:4], ["soldr", "build", "--bin", "demo-cli"])
        self.assertIn("--package", command)
        self.assertEqual(
            command[-4:], ["--profile", "dev", "--target", "x86_64-unknown-linux-gnu"]
        )
        self.assertEqual(env["RUSTC_WRAPPER"], os.environ.get("RUSTC_WRAPPER", "soldr"))
        self.assertNotIn(
            "PYO3_PYTHON", env, "a cross target must not inherit the host interpreter"
        )
        with zipfile.ZipFile(self.out / WHEEL_NAME) as archive:
            self.assertEqual(
                archive.read("demo-0.1.0.data/scripts/demo-cli"), b"demo-cli"
            )
            self.assertEqual(archive.read("demo/_bin/helper"), b"helper")

    def test_maturin_default_profile_is_mirrored_when_soldr_profile_is_disabled(
        self,
    ) -> None:
        os.environ["SOLDR_PEP517_PROFILE"] = "none"
        _, wheel_command, wheel_env = self._build("build_wheel")[0]
        self.assertEqual(wheel_command[-1], "--release")
        self.assertEqual(wheel_env["PYO3_PYTHON"], sys.executable)
        _, editable_command, _ = self._build("build_editable")[0]
        self.assertNotIn("--release", editable_command)
        self.assertNotIn("--profile", editable_command)

    def test_manifest_path_follows_tool_maturin(self) -> None:
        (self.root / "pyproject.toml").write_text(
            PYPROJECT.replace(
                'module-name = "demo._native"',
                'module-name = "demo._native"\nmanifest-path = "crates/demo/Cargo.toml"',
            ),
            encoding="utf-8",
        )
        _, command, _ = self._build("build_wheel", {"profile": "release"})[0]
        manifest = command[command.index("--manifest-path") + 1]
        self.assertEqual(Path(manifest), self.root / "crates" / "demo" / "Cargo.toml")
        self.assertEqual(command[-2:], ["--profile", "release"])

    def test_no_backend_function_is_shadowed_by_a_sibling_module(self) -> None:
        """Importing `soldr.<name>` rebinds the package attribute `<name>`.

        A backend function named like a sibling module is replaced by that
        module the first time the submodule loads, so a second hook call in the
        same process fails with "'module' object is not callable".
        """
        siblings = {path.stem for path in SRC.glob("*.py")} - {"__init__"}
        shadowed = sorted(
            name
            for name in siblings
            if isinstance(getattr(self.backend, name, None), types.FunctionType)
        )
        self.assertEqual(shadowed, [])

    def test_delegate_backend_and_bundle_bins_are_mutually_exclusive(self) -> None:
        delegate: Any = types.ModuleType("pep517_bundle_delegate")
        delegate.build_wheel = lambda *_args, **_kwargs: WHEEL_NAME
        with mock.patch.dict(sys.modules, {"pep517_bundle_delegate": delegate}):
            with mock.patch.object(
                self.backend,
                "_project_soldr_options",
                return_value={"delegate-backend": "pep517_bundle_delegate"},
            ):
                with self.assertRaises(RuntimeError) as raised:
                    self.backend.build_wheel(str(self.out))
        self.assertIn("bundle-bins", str(raised.exception))

    def test_projects_without_bundle_bins_are_untouched(self) -> None:
        (self.root / "pyproject.toml").write_text(
            "[project]\nname = 'demo'\n", encoding="utf-8"
        )
        self.assertEqual(self._build("build_wheel"), [])
        with zipfile.ZipFile(self.out / WHEEL_NAME) as archive:
            self.assertFalse(any(".data/" in name for name in archive.namelist()))


if __name__ == "__main__":
    unittest.main()
