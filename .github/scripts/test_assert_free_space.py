import shutil
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from _script_loader import load_sibling_script

assert_free_space = load_sibling_script("assert_free_space")

GIB = 1024**3


class FloorTests(unittest.TestCase):
    def test_the_archive_scaled_floor_wins_when_larger(self) -> None:
        """soldr#2933: a constant floor silently stops being enough.

        Compressed debug binaries inflate several times over, so the floor
        tracks the archive rather than a number chosen once. The archive that
        actually broke the lane was 3,302,138,143 bytes, which at 4x is still
        under the 20 GiB constant -- that is the constant doing its job, and
        `test_the_absolute_floor_wins_when_larger` covers it. This case is the
        other side: an archive large enough that a floor picked once is no
        longer enough, which is precisely the growth this scaling exists to
        survive.
        """
        floor = assert_free_space.required_floor(
            min_free_bytes=20 * GIB,
            archive_bytes=8 * GIB,
            archive_multiple=4,
        )
        self.assertEqual(floor, 32 * GIB)

    def test_the_absolute_floor_wins_when_larger(self) -> None:
        floor = assert_free_space.required_floor(
            min_free_bytes=20 * GIB, archive_bytes=1_000, archive_multiple=4
        )
        self.assertEqual(floor, 20 * GIB)

    def test_no_archive_leaves_the_absolute_floor_alone(self) -> None:
        self.assertEqual(assert_free_space.required_floor(10 * GIB, None, 4), 10 * GIB)

    def test_a_zero_multiple_disables_archive_scaling(self) -> None:
        self.assertEqual(
            assert_free_space.required_floor(10 * GIB, 99 * GIB, 0.0), 10 * GIB
        )


class NearestExistingTests(unittest.TestCase):
    def test_a_not_yet_created_extraction_dir_measures_its_parent(self) -> None:
        """The pre-extract guard runs before nextest creates the directory."""
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            missing = root / "nextest-archive" / "target" / "deps"
            self.assertEqual(assert_free_space.nearest_existing(missing), root)

    def test_an_existing_path_is_its_own_nearest(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            self.assertEqual(assert_free_space.nearest_existing(Path(raw)), Path(raw))


class EvaluateTests(unittest.TestCase):
    def test_a_reachable_floor_passes_and_names_the_volume(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            verdict = assert_free_space.evaluate(
                Path(raw), floor_bytes=1, label="pre-extract"
            )
        self.assertTrue(verdict.ok)
        self.assertIn("volume", verdict.message)
        self.assertIn("[pre-extract]", verdict.message)

    def test_an_unreachable_floor_fails_with_every_number(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            archive = root / "tests.tar.zst"
            archive.write_bytes(b"0" * 1024)
            free = shutil.disk_usage(root).free
            verdict = assert_free_space.evaluate(
                root,
                floor_bytes=free * 2 + GIB,
                label="pre-extract",
                archive=archive,
                archive_bytes=1024,
            )
        self.assertFalse(verdict.ok)
        for expected in [
            "FATAL",
            "extraction path",
            "volume",
            "free",
            "required floor",
            "archive",
            "StorageFull",
            "soldr#2933",
        ]:
            self.assertIn(expected, verdict.message)

    def test_an_unreadable_volume_is_a_failure_not_a_pass(self) -> None:
        """soldr#2933: this lane fails on disk, and a full disk is exactly
        when the measurement is most likely to misbehave. Treating 'cannot
        tell' as 'fine' would reproduce the original silence."""

        def unreadable(_path: object) -> None:
            raise OSError("device disappeared")

        with tempfile.TemporaryDirectory() as raw:
            with mock.patch.object(assert_free_space.shutil, "disk_usage", unreadable):
                verdict = assert_free_space.evaluate(Path(raw), floor_bytes=1)
        self.assertFalse(verdict.ok)
        self.assertIn("unreadable volume is treated as a failure", verdict.message)


class MainTests(unittest.TestCase):
    def test_main_returns_zero_when_the_floor_is_met(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            self.assertEqual(
                assert_free_space.main(
                    [
                        "--path",
                        str(Path(raw) / "nextest-archive"),
                        "--min-free-gib",
                        "0",
                    ]
                ),
                0,
            )

    def test_main_returns_nonzero_when_the_floor_is_missed(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            huge = shutil.disk_usage(raw).total * 4
            self.assertEqual(
                assert_free_space.main(
                    ["--path", str(raw), "--min-free-bytes", str(huge)]
                ),
                1,
            )


if __name__ == "__main__":
    unittest.main()
