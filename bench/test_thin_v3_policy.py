from __future__ import annotations

import json
import unittest
from pathlib import Path


class ThinV3PolicyTest(unittest.TestCase):
    def test_selected_strategy_is_highest_scoring_and_invariants_are_strict(self) -> None:
        """Issue #1609: the selected ownership policy must match the research."""
        root = Path(__file__).parents[1]
        policy = json.loads((root / "docs/thin_v3_policy.v1.json").read_text(encoding="utf-8"))
        scores = policy["strategy_scores"]
        weights = scores["weights"]
        totals: dict[str, int] = {}
        for name, strategy in scores.items():
            if name == "weights":
                continue
            self.assertEqual(set(strategy["ratings"]), set(weights), name)
            computed = sum(
                weight * strategy["ratings"][criterion] // 5
                for criterion, weight in weights.items()
            )
            self.assertEqual(computed, strategy["weighted_total"], name)
            totals[name] = computed

        selected = policy["selected_strategy"]
        self.assertEqual(totals[selected], max(totals.values()))
        self.assertEqual(policy["invariants"]["maximum_durable_owners_per_compiled_blob"], 1)
        self.assertEqual(
            policy["invariants"]["maximum_uploaded_copies_per_digest_per_active_lineage"],
            1,
        )
        self.assertFalse(
            policy["invariants"]["long_lived_manifest_may_reference_short_lived_remote_blob"]
        )
        self.assertEqual(
            policy["invariants"]["cross_partition_digest_collision_owner"],
            "longest-lived-owner",
        )
        self.assertEqual(
            policy["invariants"]["allowed_cross_lifetime_reference_direction"],
            "short-to-long",
        )
        self.assertTrue(policy["invariants"]["restore_chain_must_be_bounded"])
        self.assertNotEqual(
            policy["ownership_modes"]["cook-partitioned-v1"],
            policy["ownership_modes"]["zccache-all-v1"],
        )


if __name__ == "__main__":
    unittest.main()
