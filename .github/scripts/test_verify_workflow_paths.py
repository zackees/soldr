"""Tests for the workflow path-filter check.

The regression: `crates/soldr-cli/src/cache_lib/**` moved to
`crates/soldr-cache/` in #1490 Phase 4, and
`cook-size-gate.yml` kept watching the old location. Both went dark for the
code they exist to guard, and neither ever went red -- a workflow that never
triggers cannot fail.

The parser is hand-rolled (stdlib only, like `verify_ci_job_timeouts.py`), so
the YAML shapes it must survive are pinned here: quoting styles, comments,
nesting, and the `on:`-parses-as-True trap that a PyYAML version would hit.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from _script_loader import load_script_module

SCRIPT = Path(__file__).resolve().parent / "verify_workflow_paths.py"
REPO_ROOT = SCRIPT.resolve().parents[2]


@pytest.fixture(scope="module")
def mod():
    return load_script_module(SCRIPT, "verify_workflow_paths")


# --- parsing --------------------------------------------------------------


def test_reads_paths_from_a_typical_workflow(mod):
    text = """
on:
  push:
    branches: [main]
    paths:
      - "crates/soldr-cache/**"
      - 'scripts/install.js'
      - docs/API.md
jobs:
  build:
    runs-on: ubuntu-24.04
"""
    assert mod.path_filters(text) == [
        "crates/soldr-cache/**",
        "scripts/install.js",
        "docs/API.md",
    ]


def test_reads_both_paths_and_paths_ignore(mod):
    text = """
on:
  push:
    paths:
      - "a/**"
  pull_request:
    paths-ignore:
      - "**/*.md"
"""
    assert mod.path_filters(text) == ["a/**", "**/*.md"]


def test_comments_and_blank_lines_do_not_end_the_list(mod):
    text = """
on:
  push:
    paths:
      - "a/**"
      # why this one matters

      - "b/**"
"""
    assert mod.path_filters(text) == ["a/**", "b/**"]


def test_trailing_comments_are_stripped(mod):
    text = """
on:
  push:
    paths:
      - a/** # the cache crate
      - "b/**"  # quoted, comment outside
"""
    assert mod.path_filters(text) == ["a/**", "b/**"]


def test_a_following_key_ends_the_list(mod):
    # `branches:` sits at the same indent as `paths:`, so the list stops.
    text = """
on:
  push:
    paths:
      - "a/**"
    branches:
      - main
"""
    assert mod.path_filters(text) == ["a/**"]


# The indent bound in the parser (`len(indent) <= key_indent` ends the block)
# is defensive and NOT outcome-distinguishable with valid workflow YAML: any
# real continuation is either a deeper list item, which both forms accept, or a
# non-list line, which the other rule already stops on. Removing it leaves
# every test here green. Recording that rather than inventing invalid YAML to
# manufacture a kill -- the branch is cheap and correct, but nothing observes
# it, and pretending otherwise would be worse than saying so.


def test_unrelated_lists_are_not_collected(mod):
    text = """
on:
  push:
    branches:
      - main
jobs:
  build:
    steps:
      - uses: actions/checkout@v4
"""
    assert mod.path_filters(text) == []


def test_multiple_path_blocks_are_all_found(mod):
    text = """
on:
  push:
    paths:
      - "a/**"
  pull_request:
    paths:
      - "b/**"
      - "c/**"
"""
    assert mod.path_filters(text) == ["a/**", "b/**", "c/**"]


# --- matching -------------------------------------------------------------


def test_an_existing_directory_matches(mod, tmp_path):
    (tmp_path / "crates" / "soldr-cache").mkdir(parents=True)
    assert mod.matches_something("crates/soldr-cache/**", tmp_path) is True


def test_an_existing_file_matches(mod, tmp_path):
    (tmp_path / "a.txt").write_text("x", encoding="utf-8")
    assert mod.matches_something("a.txt", tmp_path) is True


def test_a_moved_path_does_not_match(mod, tmp_path):
    # The actual regression shape.
    (tmp_path / "crates" / "soldr-cache").mkdir(parents=True)
    assert mod.matches_something("crates/soldr-cli/src/cache_lib/**", tmp_path) is False


def test_a_recursive_glob_matches_nested_files(mod, tmp_path):
    nested = tmp_path / "crates" / "soldr-cache" / "src"
    nested.mkdir(parents=True)
    (nested / "lib.rs").write_text("x", encoding="utf-8")
    assert mod.matches_something("crates/**/*.rs", tmp_path) is True


def test_a_negation_is_left_alone(mod, tmp_path):
    # `!foo` refines a positive pattern; checking it for existence would
    # reject valid configuration.
    assert mod.matches_something("!crates/nope/**", tmp_path) is True


# --- end to end -----------------------------------------------------------


def test_the_real_repository_passes(mod):
    assert mod.main(["--workflows", str(REPO_ROOT / ".github" / "workflows")]) == 0


def test_a_stale_filter_fails(mod, tmp_path):
    workflows = tmp_path / "wf"
    workflows.mkdir()
    (workflows / "x.yml").write_text(
        'on:\n  push:\n    paths:\n      - "crates/gone/**"\n', encoding="utf-8"
    )
    assert mod.main(["--workflows", str(workflows), "--root", str(tmp_path)]) == 1


def test_a_live_filter_passes(mod, tmp_path):
    workflows = tmp_path / "wf"
    workflows.mkdir()
    (tmp_path / "crates").mkdir()
    (workflows / "x.yml").write_text(
        'on:\n  push:\n    paths:\n      - "crates/**"\n', encoding="utf-8"
    )
    assert mod.main(["--workflows", str(workflows), "--root", str(tmp_path)]) == 0


def test_no_workflows_at_all_is_an_error(mod, tmp_path):
    empty = tmp_path / "wf"
    empty.mkdir()
    assert mod.main(["--workflows", str(empty), "--root", str(tmp_path)]) == 1


def test_workflows_with_no_filters_at_all_is_an_error(mod, tmp_path):
    # Parsing nothing means the parser broke, not that the repo is clean.
    # Without this the check would pass hardest exactly when it stopped
    # working -- the same failure it exists to catch.
    workflows = tmp_path / "wf"
    workflows.mkdir()
    (workflows / "x.yml").write_text(
        "on:\n  push:\n    branches: [main]\n", encoding="utf-8"
    )
    assert mod.main(["--workflows", str(workflows), "--root", str(tmp_path)]) == 1


def test_the_cache_crate_is_watched_where_it_now_lives(mod):
    # The specific regression, pinned by name so a future move is noticed.
    for name in ("cook-size-gate.yml",):
        text = (REPO_ROOT / ".github" / "workflows" / name).read_text(encoding="utf-8")
        patterns = mod.path_filters(text)
        assert any(
            p.startswith("crates/soldr-cache/") for p in patterns
        ), f"{name} no longer watches the cache crate: {patterns}"
        assert not any(
            "soldr-cli/src/cache_lib" in p for p in patterns
        ), f"{name} still watches the pre-#1490 cache_lib location"
