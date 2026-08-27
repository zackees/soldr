import tempfile
import unittest
from pathlib import Path

from _script_loader import load_sibling_script

select_extract_volume = load_sibling_script("select_extract_volume")

GIB = 1024**3


def _volume(identity: str, free_gib: float) -> object:
    return select_extract_volume.Volume(
        root=Path(f"{identity}/"),
        identity=identity,
        free=int(free_gib * GIB),
        total=int((free_gib + 10) * GIB),
    )


class ChooseTests(unittest.TestCase):
    def test_the_roomiest_volume_wins(self) -> None:
        """soldr#2933: C: had 31.03 GiB and D: had 143.61 GiB untouched.

        The whole failure was that nobody compared them.
        """
        chosen = select_extract_volume.choose(
            [_volume("C:", 31.03), _volume("D:", 143.61)]
        )
        self.assertEqual(chosen.identity, "D:")

    def test_ties_break_deterministically(self) -> None:
        chosen = select_extract_volume.choose(
            [_volume("E:", 100.0), _volume("D:", 100.0)]
        )
        self.assertEqual(chosen.identity, "D:")

    def test_no_candidates_yields_none(self) -> None:
        self.assertIsNone(select_extract_volume.choose([]))


class CandidateTests(unittest.TestCase):
    def test_an_explicit_root_short_circuits_selection(self) -> None:
        override = Path("/explicit")
        roots = select_extract_volume.candidate_roots(
            Path("/ws"), Path("/tmp"), override, windows=False
        )
        self.assertEqual(roots, [override])

    def test_posix_candidates_are_deduplicated(self) -> None:
        shared = Path(tempfile.gettempdir())
        roots = select_extract_volume.candidate_roots(
            shared, shared, None, windows=False
        )
        self.assertEqual(len(roots), len(set(roots)))


class ProbeTests(unittest.TestCase):
    def test_a_missing_root_is_not_a_candidate(self) -> None:
        missing = Path(tempfile.gettempdir()) / "soldr-2933-definitely-not-here"
        self.assertIsNone(select_extract_volume.probe(missing))

    def test_a_real_directory_measures(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            measured = select_extract_volume.probe(Path(raw), 0)
        self.assertIsNotNone(measured)
        self.assertGreater(measured.total, 0)
        self.assertIn("free=", measured.describe())


class EnvLineTests(unittest.TestCase):
    def test_extraction_paths_are_always_exported(self) -> None:
        lines = select_extract_volume.env_lines(
            Path("D:/soldr-ci"), Path("D:/soldr-ci/nextest-archive"), None
        )
        self.assertEqual(
            lines,
            [
                "NEXTEST_EXTRACT_ROOT=D:/soldr-ci",
                "NEXTEST_EXTRACT_DIR=D:/soldr-ci/nextest-archive",
            ],
        )

    def test_temp_redirect_covers_all_three_spellings(self) -> None:
        """Windows reads TMP/TEMP, POSIX reads TMPDIR; a partial redirect
        leaves whichever one was missed pointing at the small volume."""
        lines = select_extract_volume.env_lines(
            Path("D:/soldr-ci"),
            Path("D:/soldr-ci/nextest-archive"),
            Path("D:/soldr-ci/tmp"),
        )
        self.assertIn("TMP=D:/soldr-ci/tmp", lines)
        self.assertIn("TEMP=D:/soldr-ci/tmp", lines)
        self.assertIn("TMPDIR=D:/soldr-ci/tmp", lines)

    def test_paths_use_forward_slashes_for_msys(self) -> None:
        """The values are consumed by `shell: bash` steps on Windows."""
        for line in select_extract_volume.env_lines(
            Path("D:/soldr-ci"), Path("D:/soldr-ci/nextest-archive"), None
        ):
            self.assertNotIn("\\", line)


class MainTests(unittest.TestCase):
    def test_an_explicit_root_is_honoured_and_exported(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            github_env = root / "github.env"
            github_env.write_text("", encoding="utf-8")
            status = select_extract_volume.main(
                [
                    "--root",
                    str(root),
                    "--prefix",
                    "soldr-ci",
                    "--name",
                    "nextest-archive",
                    "--github-env",
                    str(github_env),
                ]
            )
            self.assertEqual(status, 0)
            exported = github_env.read_text(encoding="utf-8")
            self.assertIn("NEXTEST_EXTRACT_ROOT=", exported)
            self.assertIn("NEXTEST_EXTRACT_DIR=", exported)
            self.assertTrue((root / "soldr-ci").is_dir())
            # `--extract-to` must already exist: nextest canonicalizes the
            # destination before writing, so an absent path fails the whole
            # extraction with `No such file or directory (os error 2)`.
            extract_dir = root / "soldr-ci" / "nextest-archive"
            self.assertTrue(extract_dir.is_dir())
            # ...and it must be empty, or the extraction half-merges into
            # whatever a previous attempt left behind.
            self.assertEqual(list(extract_dir.iterdir()), [])

    def test_a_stale_extraction_directory_is_cleared(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            stale = root / "soldr-ci" / "nextest-archive"
            stale.mkdir(parents=True)
            (stale / "leftover.bin").write_bytes(b"x")
            select_extract_volume.main(["--root", str(root)])
            # Cleared, then recreated empty -- not removed. nextest needs the
            # destination to exist, and needs it to hold nothing.
            self.assertTrue(stale.is_dir())
            self.assertEqual(list(stale.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
