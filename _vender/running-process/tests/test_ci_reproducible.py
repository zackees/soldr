"""Unit tests for the RUNNING_PROCESS_REPRODUCIBLE=1 build seam (#392)."""

from __future__ import annotations

import unittest
from pathlib import Path

from ci.reproducible import (
    REPRODUCIBLE_ENV_VAR,
    apply_reproducible_env,
    head_commit_epoch,
    reproducible_requested,
)

REPO_ROOT = Path(__file__).resolve().parent.parent


class TestReproducibleRequested(unittest.TestCase):
    def test_unset_is_off(self) -> None:
        self.assertFalse(reproducible_requested({}))

    def test_explicit_zero_is_off(self) -> None:
        self.assertFalse(reproducible_requested({REPRODUCIBLE_ENV_VAR: "0"}))

    def test_one_is_on(self) -> None:
        self.assertTrue(reproducible_requested({REPRODUCIBLE_ENV_VAR: "1"}))


class TestApplyReproducibleEnv(unittest.TestCase):
    def test_noop_when_seam_disabled(self) -> None:
        env = {"PATH": "/usr/bin"}
        result = apply_reproducible_env(dict(env), REPO_ROOT)
        self.assertEqual(result, env)

    def test_seam_sets_deterministic_knobs(self) -> None:
        env = {REPRODUCIBLE_ENV_VAR: "1", "RUSTFLAGS": "-Cdebuginfo=1"}
        result = apply_reproducible_env(env, REPO_ROOT)
        self.assertIn("SOURCE_DATE_EPOCH", result)
        self.assertTrue(result["SOURCE_DATE_EPOCH"].isdigit())
        self.assertEqual(result["CARGO_INCREMENTAL"], "0")
        rustflags = result["RUSTFLAGS"].split()
        # Pre-existing flags are preserved.
        self.assertIn("-Cdebuginfo=1", rustflags)
        remaps = [flag for flag in rustflags if flag.startswith("--remap-path-prefix=")]
        self.assertEqual(len(remaps), 3)
        # The workspace root remap must mention the repo root path.
        self.assertTrue(any(str(REPO_ROOT) in flag for flag in remaps))

    def test_existing_source_date_epoch_is_respected(self) -> None:
        env = {REPRODUCIBLE_ENV_VAR: "1", "SOURCE_DATE_EPOCH": "1234567890"}
        result = apply_reproducible_env(env, REPO_ROOT)
        self.assertEqual(result["SOURCE_DATE_EPOCH"], "1234567890")

    def test_idempotent(self) -> None:
        env = {REPRODUCIBLE_ENV_VAR: "1"}
        once = apply_reproducible_env(env, REPO_ROOT)
        flags_once = once["RUSTFLAGS"]
        twice = apply_reproducible_env(once, REPO_ROOT)
        self.assertEqual(twice["RUSTFLAGS"], flags_once)


class TestHeadCommitEpoch(unittest.TestCase):
    def test_epoch_is_at_least_zip_floor(self) -> None:
        self.assertGreaterEqual(head_commit_epoch(REPO_ROOT), 315532800)


if __name__ == "__main__":
    unittest.main()
