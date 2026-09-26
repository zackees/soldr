"""The repository Actions-cache budget guard (soldr#3047 Phase B).

`check_cache_budget.py` groups every live GitHub Actions cache entry into the
family whose `key_prefixes` is the longest match, then fails when an entry
matches no family, when a family exceeds its own allocation, or when the
total exceeds the hard ceiling.

`test_captured_fixture_is_red` is the acceptance item for soldr#3047: the real
`gh cache list` snapshot that motivated this guard (44.23 GiB across 143
entries) must fail it. Everything else is built from small synthetic
manifests/listings so each failure mode is exercised in isolation, in the same
style as `tests/test_cache_ownership.py`.
"""

from __future__ import annotations

import base64
import hashlib
import json
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
_SCRIPT = REPO_ROOT / ".github" / "scripts" / "check_cache_budget.py"
guard = load_script_module(_SCRIPT, "check_cache_budget")

MANIFEST = REPO_ROOT / "ci" / "cache-ownership.json"
FIXTURE = REPO_ROOT / "tests" / "fixtures" / "actions-cache" / "listing-2026-09-01.json"


def write_manifest(tmp_path: Path, budget: dict) -> Path:
    path = tmp_path / "cache-ownership.json"
    path.write_text(json.dumps({"budget": budget}), encoding="utf-8")
    return path


def write_listing(
    tmp_path: Path, entries: list[dict], name: str = "listing.json"
) -> Path:
    path = tmp_path / name
    path.write_text(
        json.dumps({"captured": "test", "usage_bytes": None, "entries": entries}),
        encoding="utf-8",
    )
    return path


def family(prefix: str, max_bytes: int) -> dict:
    return {
        "key_prefixes": [prefix],
        "max_bytes": max_bytes,
        "entries": [],
        "rationale": "fixture",
    }


def entry(key: str, size_bytes: int, ref: str = "refs/heads/main") -> dict:
    return {"key": key, "ref": ref, "sizeInBytes": size_bytes}


# --------------------------------------------------------------------------
# Acceptance: the real 2026-09-01 listing must be RED
# --------------------------------------------------------------------------


def test_captured_fixture_is_red(capsys: pytest.CaptureFixture[str]) -> None:
    code = guard.main(["--manifest", str(MANIFEST), "--from-json", str(FIXTURE)])
    assert code == 1
    out = capsys.readouterr().out
    assert "v0-rust-cross-build" in out


# --------------------------------------------------------------------------
# A synthetic listing built from the real manifest
# --------------------------------------------------------------------------


def test_half_budget_synthetic_listing_from_real_manifest_passes(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    families = manifest["budget"]["families"]

    entries = []
    for spec in families.values():
        prefix = spec["key_prefixes"][0]
        entries.append(entry(f"{prefix}half-budget-fixture", spec["max_bytes"] // 2))

    listing_path = write_listing(tmp_path, entries)
    code = guard.main(["--manifest", str(MANIFEST), "--from-json", str(listing_path)])
    assert code == 0, capsys.readouterr().out


# --------------------------------------------------------------------------
# Synthetic failures, one rule at a time
# --------------------------------------------------------------------------


def test_unregistered_key_fails(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest_path = write_manifest(
        tmp_path,
        {
            "total_max_bytes": 1000,
            "fail_total_bytes": 1000,
            "families": {"fam-a": family("a-", 500)},
        },
    )
    listing_path = write_listing(tmp_path, [entry("totally-unregistered-key-zzz", 10)])

    code = guard.main(
        ["--manifest", str(manifest_path), "--from-json", str(listing_path)]
    )
    assert code == 1
    out = capsys.readouterr().out
    assert "totally-unregistered-key-zzz" in out


def test_family_over_its_own_budget_fails(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest_path = write_manifest(
        tmp_path,
        {
            "total_max_bytes": 1000,
            "fail_total_bytes": 1000,
            "families": {"fam-a": family("a-", 100)},
        },
    )
    listing_path = write_listing(tmp_path, [entry("a-1", 150)])

    code = guard.main(
        ["--manifest", str(manifest_path), "--from-json", str(listing_path)]
    )
    assert code == 1
    out = capsys.readouterr().out
    assert "fam-a" in out


def test_families_under_but_total_over_fail_total_bytes_fails(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest_path = write_manifest(
        tmp_path,
        {
            "total_max_bytes": 2000,
            "fail_total_bytes": 1000,
            "families": {
                "fam-a": family("a-", 1000),
                "fam-b": family("b-", 1000),
            },
        },
    )
    listing_path = write_listing(
        tmp_path,
        [entry("a-1", 600), entry("b-1", 600)],
    )

    code = guard.main(
        ["--manifest", str(manifest_path), "--from-json", str(listing_path)]
    )
    assert code == 1
    out = capsys.readouterr().out
    assert "fail_total_bytes" in out


# --------------------------------------------------------------------------
# The real manifest's budget object must agree with itself
# --------------------------------------------------------------------------


def test_manifest_budget_is_self_consistent() -> None:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    budget = manifest["budget"]
    families = budget["families"]

    assert (
        sum(spec["max_bytes"] for spec in families.values())
        == budget["total_max_bytes"]
    )
    assert budget["total_max_bytes"] == 9663676416

    owned_prefixes = [
        (prefix, family_id)
        for family_id, spec in families.items()
        for prefix in spec["key_prefixes"]
    ]
    for prefix_a, family_a in owned_prefixes:
        for prefix_b, family_b in owned_prefixes:
            if family_a == family_b:
                continue
            assert not prefix_b.startswith(
                prefix_a
            ), f"{prefix_a!r} ({family_a}) is a prefix of {prefix_b!r} ({family_b})"


# --------------------------------------------------------------------------
# Network policy: skip, never fail, when gh is unavailable
# --------------------------------------------------------------------------


def test_live_mode_skips_when_gh_is_unavailable(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def boom(_args: list[str]) -> str:
        raise FileNotFoundError("gh")

    monkeypatch.setattr(guard, "run_gh", boom)

    code = guard.main(["--manifest", str(MANIFEST)])
    assert code == 0
    assert "skipped" in capsys.readouterr().out


def test_numeric_cache_ids_from_gh_survive_normalization() -> None:
    # `gh cache list --json id` returns numbers; a str-only check dropped
    # every id and made `--prune --apply` a no-op (2026-09-04).
    raw = [
        {**entry("v0-rust-x", 1, ref="refs/pull/1/merge"), "id": 7258560456},
        {**entry("v0-rust-y", 1, ref="refs/pull/1/merge"), "id": "42"},
        {**entry("v0-rust-z", 1, ref="refs/pull/1/merge"), "id": True},
    ]
    ids = [e.id for e in guard.normalize_entries(raw)]
    assert ids == ["7258560456", "42", None]


def test_apply_prune_deletes_every_candidate_by_id(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[list[str]] = []

    def record(args: list[str]) -> str:
        calls.append(args)
        return ""

    monkeypatch.setattr(guard, "run_gh", record)
    candidates = guard.normalize_entries(
        [{**entry("v0-rust-x", 1, ref="refs/pull/1/merge"), "id": 7258560456}]
    )
    assert guard.apply_prune(candidates, "zackees/soldr") == []
    assert calls == [["cache", "delete", "7258560456", "--repo", "zackees/soldr"]]


def test_prune_supersedes_older_zccache_unit_generations_on_main() -> None:
    # soldr#3102: every host-lane run on main saves a run-id keyed
    # generation of the Tier-2 store; only the newest is ever restored.
    base = "zccache-unit-v1-x86_64-unknown-linux-gnu-abc123"
    raw = [
        {**entry(f"{base}-111", 1000), "id": 1, "createdAt": "2026-09-04T01:00:00Z"},
        {**entry(f"{base}-222", 1000), "id": 2, "createdAt": "2026-09-05T01:00:00Z"},
        {**entry("stable-cook-v1-x86_64-unknown-linux-gnu-abc123", 10), "id": 3},
    ]
    candidates = guard.prune_candidates(guard.normalize_entries(raw))
    assert [c.key for c in candidates] == [f"{base}-111"]


def test_prune_supersedes_older_sha_keyed_bootstrap_drivers_on_main() -> None:
    # Exact-SHA keys with no restore-keys: an older commit's driver is only
    # restorable by re-running that commit, so each lineage keeps its newest.
    dev = "bootstrap-soldr-blessed-linux-gnu-dev-v1"
    release = "bootstrap-soldr-blessed-linux-gnu"
    raw = [
        {**entry(f"{dev}-aaa1", 23), "id": 1, "createdAt": "2026-09-13T13:45:00Z"},
        {**entry(f"{dev}-bbb2", 23), "id": 2, "createdAt": "2026-09-13T14:33:00Z"},
        {**entry(f"{dev}-ccc3", 23), "id": 3, "createdAt": "2026-09-14T01:04:00Z"},
        {**entry(f"{release}-ddd4", 19), "id": 4, "createdAt": "2026-09-12T10:00:00Z"},
        {**entry(f"{release}-eee5", 19), "id": 5, "createdAt": "2026-09-13T13:33:00Z"},
    ]
    candidates = guard.prune_candidates(guard.normalize_entries(raw))
    assert sorted(c.key for c in candidates) == sorted(
        [f"{dev}-aaa1", f"{dev}-bbb2", f"{release}-ddd4"]
    )


def test_retired_dylint_nightly_entries_are_prune_candidates() -> None:
    # soldr#3216: the nightly toolchain cache was retired; any live entry is
    # reclaimable wherever it sits.
    raw = [
        {
            **entry(
                "dylint-nightly-v1-x86_64-unknown-linux-gnu-nightly-2026-05-28", 474
            ),
            "id": 1,
        },
        {
            **entry(
                "dylint-driver-v1-x86_64-unknown-linux-gnu-nightly-2026-05-28-6.0.3", 1
            ),
            "id": 2,
        },
    ]
    candidates = guard.prune_candidates(guard.normalize_entries(raw))
    assert [c.key for c in candidates] == [
        "dylint-nightly-v1-x86_64-unknown-linux-gnu-nightly-2026-05-28"
    ]


def test_prune_supersedes_older_dylint_foundation_generations_on_main() -> None:
    # soldr#3216: the foundation key hashes the lint sources, so a lint change
    # saves a new ~413 MiB generation beside the old one.
    base = "dylint-foundation-v2-x86_64-unknown-linux-gnu"
    raw = [
        {
            **entry(f"{base}-c301e24a", 433),
            "id": 1,
            "createdAt": "2026-09-13T05:27:00Z",
        },
        {
            **entry(f"{base}-9f00aa11", 434),
            "id": 2,
            "createdAt": "2026-09-14T12:00:00Z",
        },
    ]
    candidates = guard.prune_candidates(guard.normalize_entries(raw))
    assert [c.key for c in candidates] == [f"{base}-c301e24a"]


def test_the_dylint_nightly_cache_producer_stays_retired() -> None:
    # soldr#3216: the retirement is only real if nothing re-adds the producer
    # or quietly re-registers its prefix under a family.
    families = json.loads(MANIFEST.read_text(encoding="utf-8"))["budget"]["families"]
    assert "dylint-nightly-" in guard.RETIRED_PREFIXES
    assert not any(
        prefix.startswith("dylint-nightly-")
        for spec in families.values()
        for prefix in spec["key_prefixes"]
    )
    workflow = (REPO_ROOT / ".github" / "workflows" / "_build-and-test.yml").read_text(
        encoding="utf-8"
    )
    assert "key: dylint-nightly-" not in workflow
    assert "dylint_nightly_cache" not in workflow


def test_a_lone_bootstrap_driver_per_lineage_is_kept() -> None:
    raw = [
        {**entry("bootstrap-soldr-blessed-linux-gnu-dev-v1-abc", 23), "id": 1},
        {**entry("bootstrap-soldr-blessed-linux-gnu-def", 19), "id": 2},
    ]
    assert guard.prune_candidates(guard.normalize_entries(raw)) == []


def test_cook_lock_prune_preserves_unique_shapes_and_newest_lock() -> None:
    def cook(kind: str, shape: str, lock: str, when: str, suffix: str = "") -> dict:
        return {
            **entry(
                f"cook-{kind}-v2-linux-x64-glibc-rustc1.98.1-f{shape}-l{lock}-soldr0.9.21{suffix}",
                100,
            ),
            "createdAt": when,
        }

    old = "4503d1780e10b133"
    new = "9506e5de4a14312c"
    raw = [
        cook("base", "bf1bfb42", old, "2026-09-22T01:00:00Z"),
        cook(
            "delta",
            "bf1bfb42",
            old,
            "2026-09-22T02:00:00Z",
            "-s4d428effa373-g1415998a4019694e",
        ),
        cook("base", "bf1bfb42", new, "2026-09-23T01:00:00Z"),
        cook("base", "aaa264c8", old, "2026-09-22T01:00:00Z"),
        cook("base", "aaa264c8", new, "2026-09-23T01:00:00Z"),
        cook("base", "c0f411d4", old, "2026-09-22T01:00:00Z"),  # unique shape
        cook("base", "baf623ad", new, "2026-09-23T01:00:00Z"),
        entry("unknown-cache-prefix", 100),
    ]
    candidates = guard.prune_candidates(guard.normalize_entries(raw), new)
    assert {e.key for e in candidates} == {raw[i]["key"] for i in (0, 1, 3)}


def test_stable_cook_prune_keeps_both_without_source_hash() -> None:
    raw = [
        {
            **entry("stable-cook-v2-x86_64-unknown-linux-gnu-" + "a" * 64, 100),
            "createdAt": "2026-09-22T01:00:00Z",
        },
        {
            **entry("stable-cook-v2-x86_64-unknown-linux-gnu-" + "b" * 64, 100),
            "createdAt": "2026-09-23T01:00:00Z",
        },
        {
            **entry("stable-cook-v2-aarch64-unknown-linux-gnu-" + "c" * 64, 100),
            "createdAt": "2026-09-22T01:00:00Z",
        },
    ]
    entries = guard.normalize_entries(raw)
    assert guard.prune_candidates(entries) == []
    assert guard.prune_candidates(entries, stable_cook_source_hash="invalid") == []
    assert [
        e.key for e in guard.prune_candidates(entries, stable_cook_source_hash="b" * 64)
    ] == [raw[0]["key"]]
    # Source rollback: the older-created archive is current; the newer one
    # may be retired only because GitHub's exact source hash says so.
    assert [
        e.key for e in guard.prune_candidates(entries, stable_cook_source_hash="a" * 64)
    ] == [raw[1]["key"]]
    # Another target's unique archive is always retained.
    assert guard.prune_candidates(entries, stable_cook_source_hash="d" * 64) == []


def test_3347_active_generations_need_lineage_and_producer_shrink() -> None:
    # Approximate the 20:26 listing in MiB; five current shapes, two old
    # shapes, PR copies, two unit runs, two stable-cook hashes, and residual.
    mib = 1024**2
    rows: list[dict] = []
    shapes = ("bf1bfb42", "c0f411d4", "8a8107f7", "baf623ad", "aaa264c8")
    for shape in shapes:
        rows.append(
            {
                **entry(
                    f"cook-base-v2-linux-x64-glibc-rustc1.98.1-f{shape}-l9506e5de4a14312c-soldr0.9.21",
                    430 * mib,
                ),
                "createdAt": "2026-09-23T14:00:00Z",
            }
        )
    for shape in shapes[:2]:
        rows.append(
            {
                **entry(
                    f"cook-base-v2-linux-x64-glibc-rustc1.98.1-f{shape}-l4503d1780e10b133-soldr0.9.21",
                    390 * mib,
                ),
                "createdAt": "2026-09-22T14:00:00Z",
            }
        )
    for shape in shapes[:3]:
        rows.append(
            entry(
                f"cook-base-v2-linux-x64-glibc-rustc1.98.1-f{shape}-l4503d1780e10b133-soldr0.9.21",
                400 * mib,
                "refs/pull/3346/merge",
            )
        )
    rows += [
        {
            **entry("zccache-unit-v1-linux-abc-111", 1300 * mib),
            "createdAt": "2026-09-22T01:00:00Z",
        },
        {
            **entry("zccache-unit-v1-linux-abc-222", 1300 * mib),
            "createdAt": "2026-09-23T01:00:00Z",
        },
        {
            **entry("stable-cook-v2-x86_64-unknown-linux-gnu-" + "a" * 64, 300 * mib),
            "createdAt": "2026-09-22T01:00:00Z",
        },
        {
            **entry("stable-cook-v2-x86_64-unknown-linux-gnu-" + "b" * 64, 650 * mib),
            "createdAt": "2026-09-23T01:00:00Z",
        },
        entry("v0-rust-bootstrap-soldr-linux-gnu-dev-abc", 402 * mib),
        entry("v0-rust-wheel-cross-aarch64-unknown-linux-gnu-release-abc", 602 * mib),
    ]
    entries = guard.normalize_entries(rows)
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    assert guard.budget_problems(MANIFEST, manifest, entries)
    # The existing policy could only reclaim PR copies and the older unit run.
    old_candidates = [
        e for e in entries if e.ref != "refs/heads/main" or e.key.endswith("-111")
    ]
    assert guard.budget_problems(
        MANIFEST, manifest, [e for e in entries if e not in old_candidates]
    )
    candidates = guard.prune_candidates(entries, "9506e5de4a14312c", "b" * 64)
    effective = [e for e in entries if e not in candidates]
    problems = guard.budget_problems(MANIFEST, manifest, effective)
    assert (
        len(candidates) == 7
    )  # PR bases, old cook locks, old unit and stable generations
    assert any("rust-cache-residual" in p for p in problems)
    assert not any("zccache-unit" in p for p in problems)
    # Only after the residual producer shrinks does every family fit.
    shrunk = [
        (
            e
            if not e.key.startswith("v0-rust-wheel-cross-")
            else guard.CacheEntry(e.key, e.ref, 560 * mib)
        )
        for e in effective
    ]
    assert guard.budget_problems(MANIFEST, manifest, shrunk) == []


def test_cook_rollback_uses_main_lock_not_creation_time() -> None:
    base = "cook-base-v2-linux-x64-glibc-rustc1.98.1-fbf1bfb42-l{}-soldr0.9.21"
    old = {
        **entry(base.format("4503d1780e10b133"), 100),
        "createdAt": "2026-09-24T01:00:00Z",
    }
    current = {
        **entry(base.format("9506e5de4a14312c"), 100),
        "createdAt": "2026-09-23T01:00:00Z",
    }
    entries = guard.normalize_entries([old, current])
    assert [e.key for e in guard.prune_candidates(entries, "9506e5de4a14312c")] == [
        old["key"]
    ]
    assert [e.key for e in guard.prune_candidates(entries, "4503d1780e10b133")] == [
        current["key"]
    ]
    assert guard.prune_candidates(entries) == []


def test_main_lock_hash_uses_remote_raw_bytes(monkeypatch: pytest.MonkeyPatch) -> None:
    lock_bytes = b"# exact source bytes\nversion = 4\n"
    response = json.dumps(
        {"encoding": "base64", "content": base64.b64encode(lock_bytes).decode()}
    )
    calls: list[list[str]] = []

    def fake_gh(args: list[str]) -> str:
        calls.append(args)
        return response

    monkeypatch.setattr(guard, "run_gh", fake_gh)
    assert (
        guard.fetch_main_lock_hash("zackees/soldr")
        == hashlib.sha256(lock_bytes).hexdigest()[:16]
    )
    assert calls == [["api", "repos/zackees/soldr/contents/Cargo.lock?ref=main"]]


def test_main_sha_resolves_live_ref(monkeypatch: pytest.MonkeyPatch) -> None:
    sha = "a" * 40
    monkeypatch.setattr(
        guard,
        "run_gh",
        lambda args: (
            json.dumps({"object": {"sha": sha}})
            if args == ["api", "repos/zackees/soldr/git/ref/heads/main"]
            else "{}"
        ),
    )
    assert guard.fetch_main_sha("zackees/soldr") == sha


def test_stable_cook_source_hash_matches_producer_expression() -> None:
    producer = (REPO_ROOT / ".github/workflows/_build-and-test.yml").read_text()
    sweep = (REPO_ROOT / ".github/workflows/cache-budget.yml").read_text()
    expression = "hashFiles('Cargo.lock', 'Cargo.toml', 'crates/*/Cargo.toml', 'rust-toolchain.toml', '.cargo/config.toml')"
    assert expression in producer
    assert expression in sweep
    assert "if: github.ref == 'refs/heads/main'" in sweep


# --------------------------------------------------------------------------
# soldr#3347: main cook lock-generation lineage, RED -> GREEN
# --------------------------------------------------------------------------

LINEAGE_FIXTURE = (
    REPO_ROOT
    / "tests"
    / "fixtures"
    / "actions-cache"
    / "listing-3347-cook-lineage.json"
)
CURRENT_LOCK = "e8c3129b32c03b91"
PRIOR_LOCK = "9506e5de4a14312c"
CURRENT_STABLE = "b" * 64


def lineage_entries() -> list:
    raw, _ = guard.load_from_json(LINEAGE_FIXTURE)
    return guard.normalize_entries(raw)


def legacy_cook_candidates(on_main: list, lock: str) -> list:
    """The pre-#3347 rule: a shape matched only at the SAME soldr version."""
    bases = set()
    for e in on_main:
        m = guard.COOK_KEY.fullmatch(e.key)
        if m and m.group(1) == "base" and m.group(3) == lock:
            bases.add((m.group(2), m.group(4)))
    return [
        e
        for e in on_main
        if (m := guard.COOK_KEY.fullmatch(e.key))
        and (m.group(2), m.group(4)) in bases
        and m.group(3) != lock
    ]


def without(entries: list, dropped: list) -> list:
    ids = {id(e) for e in dropped}
    return [e for e in entries if id(e) not in ids]


def test_3347_fixture_is_red_raw() -> None:
    entries = lineage_entries()
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    problems = guard.budget_problems(MANIFEST, manifest, entries)
    assert any("'cook-layer'" in p for p in problems)
    assert any("'zccache-unit'" in p for p in problems)


def test_3347_legacy_policy_still_fails_after_its_reclaim() -> None:
    entries = lineage_entries()
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    everything = guard.prune_candidates(entries, CURRENT_LOCK, CURRENT_STABLE)
    new_cook = guard.cook_lineage_candidates(
        [e for e in entries if e.ref == "refs/heads/main"], CURRENT_LOCK
    )
    legacy = without(everything, new_cook) + legacy_cook_candidates(
        [e for e in entries if e.ref == "refs/heads/main"], CURRENT_LOCK
    )
    problems = guard.budget_problems(MANIFEST, manifest, without(entries, legacy))
    assert any("'cook-layer'" in p for p in problems)
    assert guard.effective_verdict(problems).startswith(
        "effective after safe prune: STILL OVER BUDGET"
    )


def test_3347_lineage_policy_fits_every_family_and_total() -> None:
    entries = lineage_entries()
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    candidates = guard.prune_candidates(entries, CURRENT_LOCK, CURRENT_STABLE)
    effective = without(entries, candidates)
    assert guard.budget_problems(MANIFEST, manifest, effective) == []
    assert guard.effective_verdict([]).endswith("every family and the total fit")
    kept_cook = {e.key for e in effective if e.key.startswith("cook-")}
    # Exactly the current lock generation for every one of the five shapes.
    assert len(kept_cook) == 10
    assert all(f"-l{CURRENT_LOCK}-" in k for k in kept_cook)
    assert len({id(e) for e in candidates}) == len(candidates)  # no double count


def test_3347_policy_is_not_green_when_a_family_truly_does_not_fit() -> None:
    # The residual producer sizes from the 2026-09-23 20:26 listing were
    # 36,875,420 B over; no safe candidate touches them, so it must stay red.
    entries = [
        (
            guard.CacheEntry(e.key, e.ref, 635_272_319, e.id, e.created_at)
            if e.key.startswith("v0-rust-wheel-cross-")
            else e
        )
        for e in lineage_entries()
    ]
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    effective = without(
        entries, guard.prune_candidates(entries, CURRENT_LOCK, CURRENT_STABLE)
    )
    problems = guard.budget_problems(MANIFEST, manifest, effective)
    assert any("'rust-cache-residual'" in p for p in problems)
    assert "STILL OVER BUDGET" in guard.effective_verdict(problems)


def _cook(kind: str, shape: str, lock: str, soldr: str, suffix: str = "") -> dict:
    return entry(
        f"cook-{kind}-v2-linux-x64-glibc-rustc1.98.1-f{shape}-l{lock}-soldr{soldr}{suffix}",
        100,
    )


def test_3347_never_retires_a_unique_active_shape() -> None:
    raw = [
        _cook("base", "aaa264c8", CURRENT_LOCK, "0.9.22"),
        _cook("base", "c0f411d4", PRIOR_LOCK, "0.9.21"),  # no current base
        _cook("delta", "c0f411d4", PRIOR_LOCK, "0.9.21", "-s1-g2"),
    ]
    assert guard.prune_candidates(guard.normalize_entries(raw), CURRENT_LOCK) == []


def test_3347_never_retires_an_unknown_prefix() -> None:
    raw = [
        _cook("base", "aaa264c8", CURRENT_LOCK, "0.9.22"),
        entry(
            f"cook-base-v3-linux-x64-glibc-rustc1.98.1-faaa264c8-l{PRIOR_LOCK}-soldr0.9.21",
            100,
        ),
        entry("mystery-cache-faaa264c8-l" + PRIOR_LOCK, 100),
    ]
    assert guard.prune_candidates(guard.normalize_entries(raw), CURRENT_LOCK) == []


def test_3347_keeps_every_required_current_lock_generation() -> None:
    raw = [
        _cook("base", "aaa264c8", CURRENT_LOCK, "0.9.22"),
        _cook("delta", "aaa264c8", CURRENT_LOCK, "0.9.22", "-s1-g2"),
        # Same lock, older soldr: still required for a soldr rollback.
        _cook("base", "aaa264c8", CURRENT_LOCK, "0.9.21"),
        _cook("base", "aaa264c8", PRIOR_LOCK, "0.9.21"),
    ]
    entries = guard.normalize_entries(raw)
    assert [e.key for e in guard.prune_candidates(entries, CURRENT_LOCK)] == [
        raw[3]["key"]
    ]
    # Unknown current lock: the required generation cannot be identified.
    assert guard.prune_candidates(entries, None) == []
