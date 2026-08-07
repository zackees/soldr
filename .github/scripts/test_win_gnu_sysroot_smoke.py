#!/usr/bin/env python3
"""Pure tests for the win-gnu host-neutral sysroot smoke verifier."""

from __future__ import annotations

import os
import tempfile
import unittest

from _script_loader import load_sibling_script

script = load_sibling_script("win_gnu_sysroot_smoke")


def _touch(path: str) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("stub")


def _make_complete_package(package: str) -> None:
    prefix = script.TARGET_PREFIX
    for rel in [
        f"{prefix}/include/windows.h",
        f"{prefix}/lib/libkernel32.a",
        f"{prefix}/lib/libmingw32.a",
        f"{prefix}/lib/libmsvcrt.a",
        f"{prefix}/lib/crt2.o",
        f"lib/gcc/{prefix}/15.3.0/libgcc.a",
    ]:
        _touch(os.path.join(package, rel))


class VerifyTests(unittest.TestCase):
    def test_complete_package_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            package = os.path.join(tmp, "package")
            _make_complete_package(package)
            rows = script.check(package)
            self.assertTrue(all(ok for _, ok, _ in rows), rows)

    def test_versioned_libgcc_is_globbed(self) -> None:
        # The gcc runtime nests a per-version subdir; a different version
        # must still resolve via the glob.
        with tempfile.TemporaryDirectory() as tmp:
            package = os.path.join(tmp, "package")
            _make_complete_package(package)
            # Move libgcc.a into a differently-named version dir.
            prefix = script.TARGET_PREFIX
            os.remove(os.path.join(package, f"lib/gcc/{prefix}/15.3.0/libgcc.a"))
            _touch(os.path.join(package, f"lib/gcc/{prefix}/14.0.0/libgcc.a"))
            rows = script.check(package)
            self.assertTrue(all(ok for _, ok, _ in rows), rows)

    def test_missing_import_lib_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            package = os.path.join(tmp, "package")
            _make_complete_package(package)
            os.remove(
                os.path.join(package, script.TARGET_PREFIX, "lib", "libkernel32.a")
            )
            rows = script.check(package)
            missing = [rel for rel, ok, _ in rows if not ok]
            self.assertIn(f"{script.TARGET_PREFIX}/lib/libkernel32.a", missing)

    def test_missing_libgcc_glob_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            package = os.path.join(tmp, "package")
            _make_complete_package(package)
            prefix = script.TARGET_PREFIX
            os.remove(os.path.join(package, f"lib/gcc/{prefix}/15.3.0/libgcc.a"))
            rows = script.check(package)
            missing = [rel for rel, ok, _ in rows if not ok]
            self.assertIn(f"lib/gcc/{prefix}/*/libgcc.a", missing)


class GccProfileTests(unittest.TestCase):
    def test_gcc_profile_requires_bin_executables(self) -> None:
        prefix = script.TARGET_PREFIX
        with tempfile.TemporaryDirectory() as tmp:
            package = os.path.join(tmp, "package")
            for rel in [
                "bin/gcc.exe",
                "bin/dlltool.exe",
                "bin/windres.exe",
                f"{prefix}/include/windows.h",
                f"{prefix}/lib/libkernel32.a",
            ]:
                _touch(os.path.join(package, rel))
            rows = script.check(package, "mingw-w64-gcc")
            self.assertTrue(all(ok for _, ok, _ in rows), rows)
            # Dropping dlltool (the soldr#2336 item-4 tool) fails the profile.
            os.remove(os.path.join(package, "bin", "dlltool.exe"))
            rows = script.check(package, "mingw-w64-gcc")
            self.assertIn("bin/dlltool.exe", [rel for rel, ok, _ in rows if not ok])

    def test_unknown_tool_errors(self) -> None:
        with self.assertRaises(SystemExit):
            script.check("/nonexistent", "not-a-tool")


class LocateTests(unittest.TestCase):
    def test_locate_finds_single_versioned_package(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            package = os.path.join(
                home,
                "bin",
                "syslib",
                script.TOOL,
                "15.3.0posix-14.0.0-msvcrt-r1",
                script.SLUG,
                "package",
            )
            _make_complete_package(package)
            self.assertEqual(script.locate_package(home), package)

    def test_locate_errors_when_absent(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            with self.assertRaises(SystemExit):
                script.locate_package(home)

    def test_locate_errors_on_ambiguity(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            for version in ("15.3.0posix-14.0.0-msvcrt-r1", "16.0.0posix"):
                package = os.path.join(
                    home, "bin", "syslib", script.TOOL, version, script.SLUG, "package"
                )
                _make_complete_package(package)
            with self.assertRaises(SystemExit):
                script.locate_package(home)


if __name__ == "__main__":
    unittest.main()
