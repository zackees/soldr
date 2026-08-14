"""The CI log index must name every log without printing any of them.

Guards `.github/scripts/index_build_logs.py`, which replaced two steps that
dumped whole compile journals into the job console (soldr#2493 follow-up). The
regression this locks down is twofold: contents must never reach stdout, and
the three log families that live *outside* a `logs/` directory -- archived
compile journals, spawn logs, and the broker bringup timings -- must still be
discovered.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = Path(__file__).resolve().parents[1] / ".github" / "scripts" / "index_build_logs.py"


@pytest.fixture(scope="module")
def indexer():
    return load_script_module(SCRIPT)


def test_indexes_logs_outside_a_logs_directory(indexer, tmp_path):
    """The paths that were silently missing before must be discovered."""
    (tmp_path / "logs" / "builds").mkdir(parents=True)
    (tmp_path / "logs" / "builds" / "build.xml").write_text("<build/>", encoding="utf-8")
    (tmp_path / "cache" / "zccache" / "history" / "17").mkdir(parents=True)
    (tmp_path / "cache" / "zccache" / "history" / "17" / "compile_journal.jsonl").write_text(
        '{"unit":"a"}\n', encoding="utf-8"
    )
    (tmp_path / "broker").mkdir()
    (tmp_path / "broker" / "broker-spawn.log").write_text("bound\n", encoding="utf-8")
    (tmp_path / "broker" / "broker-bringup.jsonl").write_text('{"phase":"bind"}\n', encoding="utf-8")

    found = {path.name for path in indexer.discover(tmp_path)}
    assert found == {
        "build.xml",
        "compile_journal.jsonl",
        "broker-spawn.log",
        "broker-bringup.jsonl",
    }


def test_index_never_prints_file_contents(indexer, tmp_path):
    """The whole point: names and sizes, not bodies."""
    (tmp_path / "logs").mkdir()
    (tmp_path / "logs" / "compile_journal.jsonl").write_text(
        '{"secret_unit":"must-not-be-printed"}\n', encoding="utf-8"
    )

    rendered = indexer.render([tmp_path], "build-logs-linux")
    assert "compile_journal.jsonl" in rendered
    assert "must-not-be-printed" not in rendered
    assert "build-logs-linux" in rendered


def test_missing_root_is_reported_not_fatal(indexer, tmp_path):
    """`if: always()` steps run on lanes that never created a soldr home."""
    rendered = indexer.render([tmp_path / "absent"], None)
    assert "(none)" in rendered
    assert indexer.main(["--root", str(tmp_path / "absent")]) == 0


def test_build_state_directories_are_not_walked(indexer, tmp_path):
    """Indexing must not degrade into a listing of installed toolchains."""
    (tmp_path / "toolchains" / "1.95.0").mkdir(parents=True)
    (tmp_path / "toolchains" / "1.95.0" / "manifest.json").write_text("{}", encoding="utf-8")
    (tmp_path / "logs").mkdir()
    (tmp_path / "logs" / "build.xml").write_text("<build/>", encoding="utf-8")

    found = {path.name for path in indexer.discover(tmp_path)}
    assert found == {"build.xml"}
