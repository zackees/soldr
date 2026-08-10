from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from ci import build_wheel


class PreserveDevPdbTest(unittest.TestCase):
    def test_keeps_the_exact_wheel_build_artifact(self) -> None:
        triple = "x86_64-pc-windows-msvc"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "target" / triple / "debug" / "_native.pdb"
            source.parent.mkdir(parents=True)
            source.write_bytes(b"exact-codeview-identity")
            with (
                patch.object(build_wheel, "ROOT", root),
                patch("ci.env.host_target_triple", return_value=triple),
            ):
                preserved = build_wheel.preserve_dev_pdb()

            self.assertEqual(
                preserved, root / "target" / "probe-symbols" / triple / "_native.pdb"
            )
            self.assertEqual(preserved.read_bytes(), source.read_bytes())
