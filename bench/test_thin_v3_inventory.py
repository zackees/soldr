from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

import thin_v3_inventory


class ThinV3InventoryTest(unittest.TestCase):
    def test_reports_cross_layer_duplicate_bytes_and_classes(self) -> None:
        """Issue #1609: duplicate bytes must be attributed across owners."""
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            first = root / "first"
            second = root / "second"
            (first / "debug/deps").mkdir(parents=True)
            (second / "artifacts").mkdir(parents=True)
            payload = b"same compiler output"
            (first / "debug/deps/libdemo.rlib").write_bytes(payload)
            (second / "artifacts/key_0").write_bytes(payload)
            (first / "debug/deps/demo.d").write_text("source.rs", encoding="utf-8")

            report = thin_v3_inventory.build_report([("thin", first), ("zccache", second)])

            self.assertEqual(report["combined"]["duplicate_bytes"], len(payload))
            self.assertEqual(report["combined"]["duplicate_file_count"], 1)
            self.assertEqual(report["layers"]["thin"]["classes"]["rlib"]["file_count"], 1)
            self.assertEqual(report["layers"]["thin"]["classes"]["dep_info"]["file_count"], 1)


if __name__ == "__main__":
    unittest.main()
