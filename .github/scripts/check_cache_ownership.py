#!/usr/bin/env python3
"""Fail CI when a normal workflow persists a linked test product (soldr#2931).

soldr#2937 is phase 5 of soldr#2931. The policy it enforces is that cache
admission follows the stability of an artifact's identity key relative to its
size:

* **Tier 1 `cook`** -- external dependency compilation plus build-script /
  native outputs. Durable.
* **Tier 2 `zccache-unit`** -- per-compilation-unit, content-addressed
  compiler outputs. Durable, and safe by construction because the key *is*
  the hash of the compiler inputs.
* **Tier 3 `none`** -- everything else. In particular **linked test products
  are never cacheable**: test binaries, benches, examples built for tests,
  doctest products, test debug sidecars and test-specific incremental state
  may not enter cook, an rlib cache, a broad `target/` cache, a
  compiler-cache archive, or any other cross-run reusable store.

## The failure mode this prevents

A linked test binary is enormous and its identity key is the least stable
thing in the build: it moves with every source edit in the workspace. Storing
it buys a hit rate near zero while paying full upload, download and eviction
cost on every run -- and worse, it *displaces* entries whose keys are stable
(dependency rlibs, cc-rs outputs), so the store gets slower at the job it is
good at. soldr#1391 is the worked example in the other direction: a guard was
built that *required* the full linked nextest archive to be warm-cacheable at
zero misses. That invariant is now inverted, and this guard is what stops it
from creeping back in via a workflow edit.

The second rule covers the same damage arriving by a different route. Cook
restores a dependency-scoped tree. Restoring a **broad `target/` snapshot
after** it overwrites that tree with a bulk copy whose key is a whole-workspace
hash -- test binaries and `incremental/` state included. The cook restore then
bought nothing, and the store now carries exactly what tier 3 forbids. Order
matters: a broad restore *before* cook is fine, which is the shape
`_ci-cross-build-linux.yml` already uses.

## What is NOT flagged

Content-addressed per-unit caching stays live. soldr#2931 bans persisting
*linked products* and *bulk `target/` snapshots*; it does not disable zccache
dogfooding, and the soldr#1698 Windows cache-roundtrip probes depend on the
embedded wrapper being active in normal lanes. `zccache-unit` stores are
therefore never flagged by this guard.

`upload-artifact` / `download-artifact` are not cross-run reusable stores:
they are keyed by run id, never restored into a later run, and expire on
retention. A same-run transport bundle carrying cross-built test products to a
native runner is legal -- that is how `_ci-cross-build-linux.yml` reaches
`_ci-target-run.yml`. Its size budget is owned by those lanes, not here.

## Rules

R1  The manifest (`ci/cache-ownership.json`) parses, declares
    `schema_version: 1`, has unique ids, uses only declared tiers and
    exceptions, and its `experiment_workflows` list matches
    `EXPERIMENT_WORKFLOWS` below exactly.
R2  Every workflow that persists something is covered by a manifest entry,
    and every manifest entry names a workflow that still persists something.
R3  In a non-exempt workflow, no durable cross-run store may name a test
    product (test/bench/example/doctest outputs, test debug sidecars,
    `incremental/` state, a nextest test archive).
R4  In a non-exempt workflow, no job may restore a broad `target/` snapshot
    *after* a cook restore in the same job.
R5  A manifest entry tiered `same-run-transport` must use an artifact
    mechanism, and a durable-cache mechanism may not claim that tier.

Usage:
    python .github/scripts/check_cache_ownership.py
Options:
    --manifest PATH       policy manifest (default: ci/cache-ownership.json)
    --workflow-dir PATH   workflows to scan (default: .github/workflows)
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
from dataclasses import dataclass

import yaml

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOW_DIR = REPO_ROOT / ".github" / "workflows"
MANIFEST = REPO_ROOT / "ci" / "cache-ownership.json"

SCHEMA_VERSION = 1

# The cache IS the subject under test in these workflows, so constraining what
# they persist would destroy the measurement. Frozen here rather than read from
# the manifest alone: a guard whose exemption list can be edited in the same
# file it guards is a guard that can be silently switched off. R1 requires the
# manifest to agree with this list, so the two can only change together.
EXPERIMENT_WORKFLOWS: frozenset[str] = frozenset(
    {
        "baseline-zero-deps.yml",
        "cache-delta-experiment.yml",
        "cook-size-gate.yml",
        "dylint-cache-acceptance.yml",
        "dylint-cook-acceptance.yml",
        "nextest-cacheability.yml",
        "parent-cache-bench.yml",
        "perf-cold-warm.yml",
        "perf-matrix.yml",
    }
)

# `uses:` values (SHA stripped, lowercased) that persist something, mapped to
# the mechanism name the manifest uses.
PERSISTING_ACTIONS: dict[str, str] = {
    "actions/cache": "actions/cache",
    "actions/cache/restore": "actions/cache",
    "actions/cache/save": "actions/cache",
    "actions/upload-artifact": "actions/upload-artifact",
    "actions/download-artifact": "actions/download-artifact",
    "swatinem/rust-cache": "Swatinem/rust-cache",
    "zackees/setup-soldr/cook": "setup-soldr/cook",
}

# setup-soldr gates each reuse layer on its own input, NOT on the `cache`
# umbrella (soldr#2451), so a layer counts as persisting only when a workflow
# turns it on explicitly. Every self-build lane in this repo spells out
# `false` for all of them; those spellings deliberately produce no entry.
SETUP_SOLDR_ACTION = "zackees/setup-soldr"
SETUP_SOLDR_LAYERS: tuple[str, ...] = (
    "cache",
    "build-cache",
    "target-cache",
    "cargo-registry-cache",
    "soldr-mini-cache",
    "solo-toolchain-cache",
    "ci-tests",
)

# Mechanisms that persist across runs and can therefore be *reused* by a later
# build. Artifact upload/download is excluded on purpose: it is run-keyed.
DURABLE_MECHANISMS: frozenset[str] = frozenset(
    {"actions/cache", "Swatinem/rust-cache", "setup-soldr/cook"}
)

ARTIFACT_MECHANISMS: frozenset[str] = frozenset(
    {"actions/upload-artifact", "actions/download-artifact"}
)

# Tier 3 shapes, named so the error message says what was found rather than
# which regex fired. Matched against a step's `path` / `flags` / `key` text.
TEST_PRODUCT_PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("a nextest test archive", re.compile(r"tests?\.tar", re.IGNORECASE)),
    ("a nextest archive", re.compile(r"nextest[-_ ]archive", re.IGNORECASE)),
    (
        "compiled test/bench/example unit outputs",
        re.compile(r"target/[^\s'\"]*/(deps|examples|benches)(/|\b)", re.IGNORECASE),
    ),
    ("cargo incremental state", re.compile(r"\bincremental\b", re.IGNORECASE)),
    ("doctest products", re.compile(r"doctest", re.IGNORECASE)),
    ("test debug sidecars", re.compile(r"\.(pdb|dsym|dwp)\b", re.IGNORECASE)),
    (
        "linked test binaries",
        re.compile(r"test[-_]?(bin|binaries|harness|exe)", re.IGNORECASE),
    ),
)

# A path that names the whole cargo target directory rather than a slice of it.
BROAD_TARGET_PATHS: frozenset[str] = frozenset(
    {"target", "target/", "./target", "./target/", "target/**", "./target/**"}
)


@dataclass(frozen=True)
class PersistedStep:
    """One workflow step that writes to or reads from a persistent store."""

    workflow: str
    job: str
    index: int
    name: str
    mechanism: str
    text: str

    @property
    def durable(self) -> bool:
        """Whether a later run can reuse what this step persists.

        Every enabled `setup-soldr:<layer>` counts. The zccache layers are
        content-addressed per unit, so tier 3 cannot normally pollute them --
        a unit key is the hash of that unit's compiler inputs. They are still
        checked here because a *path* naming a test product would mean the
        layer had been pointed somewhere it does not belong.
        """
        return self.mechanism in DURABLE_MECHANISMS or self.mechanism.startswith(
            "setup-soldr:"
        )

    def describe(self) -> str:
        label = self.name or f"(unnamed step #{self.index})"
        return f"{self.workflow}: step '{label}' ({self.mechanism})"


def action_id(uses: str) -> str:
    """`owner/repo/sub@sha` -> `owner/repo/sub`, lowercased."""
    return uses.split("@", 1)[0].strip().rstrip("/").lower()


def _truthy(value: object) -> bool:
    """Whether a `with:` input turns a layer on.

    YAML gives `true` as a bool and `'true'` as a string, and workflows in
    this repo use both spellings; an expression like `${{ ... && 'false' ||
    'true' }}` is neither, and is treated as on because it *can* be on.
    """
    if isinstance(value, bool):
        return value
    text = str(value).strip().lower()
    if text in {"false", "off", "no", "0", "", "none"}:
        return False
    return True


def step_text(step: dict) -> str:
    """The step inputs a path/key rule needs to inspect, joined."""
    with_block = step.get("with")
    if not isinstance(with_block, dict):
        return ""
    keys = (
        "path",
        "flags",
        "key",
        "restore-keys",
        "shared-key",
        "target-dir",
        "target-cache-mode",
        "cache-dir",
    )
    return "\n".join(str(with_block[k]) for k in keys if with_block.get(k) is not None)


def collect_steps(workflow_dir: pathlib.Path) -> list[PersistedStep]:
    """Every persisting step in every workflow, in file and step order."""
    found: list[PersistedStep] = []
    paths = sorted(workflow_dir.glob("*.yml")) + sorted(workflow_dir.glob("*.yaml"))
    for path in paths:
        document = yaml.safe_load(path.read_text(encoding="utf-8"))
        if not isinstance(document, dict):
            continue
        for job_id, job in (document.get("jobs") or {}).items():
            if not isinstance(job, dict):
                continue
            for index, step in enumerate(job.get("steps") or []):
                if not isinstance(step, dict):
                    continue
                found.extend(
                    _classify(path.name, str(job_id), index, step),
                )
    return found


def _classify(workflow: str, job: str, index: int, step: dict) -> list[PersistedStep]:
    uses = step.get("uses")
    if not isinstance(uses, str):
        return []
    identifier = action_id(uses)
    name = str(step.get("name") or "")
    text = step_text(step)

    mechanism = PERSISTING_ACTIONS.get(identifier)
    if mechanism is not None:
        return [PersistedStep(workflow, job, index, name, mechanism, text)]

    if identifier != SETUP_SOLDR_ACTION:
        return []

    with_block = step.get("with")
    if not isinstance(with_block, dict):
        return []
    enabled = [
        layer
        for layer in SETUP_SOLDR_LAYERS
        if layer in with_block and _truthy(with_block[layer])
    ]
    # `prebuild-deps: soldr-cook` runs setup-soldr's early cook. No workflow
    # selects it today (both callers pass `none`), but it is the same store as
    # the `setup-soldr/cook` subaction and must be classified if it returns.
    prebuild = str(with_block.get("prebuild-deps", "none")).strip().lower()
    if prebuild not in {"none", "false", ""}:
        enabled.append("cook")
    return [
        PersistedStep(workflow, job, index, name, f"setup-soldr:{layer}", text)
        for layer in enabled
    ]


# --------------------------------------------------------------------------
# R1 / R5 -- the manifest itself
# --------------------------------------------------------------------------


def manifest_problems(manifest: dict) -> list[str]:
    """Schema, vocabulary and exemption-agreement failures (R1, R5)."""
    problems: list[str] = []

    version = manifest.get("schema_version")
    if version != SCHEMA_VERSION:
        problems.append(
            f"R1 manifest schema_version is {version!r}, expected {SCHEMA_VERSION}"
        )

    tiers = manifest.get("tiers")
    exceptions = manifest.get("exceptions")
    if not isinstance(tiers, dict) or not isinstance(exceptions, dict):
        problems.append(
            "R1 manifest must declare object-valued 'tiers' and 'exceptions'"
        )
        return problems
    vocabulary = set(tiers) | set(exceptions)

    declared = manifest.get("experiment_workflows")
    if not isinstance(declared, list) or set(declared) != set(EXPERIMENT_WORKFLOWS):
        problems.append(
            "R1 manifest 'experiment_workflows' must match EXPERIMENT_WORKFLOWS in "
            f"{pathlib.Path(__file__).name} exactly.\n"
            f"     manifest: {sorted(declared) if isinstance(declared, list) else declared}\n"
            f"     guard:    {sorted(EXPERIMENT_WORKFLOWS)}\n"
            "     Both must change together, so the exemption list cannot be "
            "widened by editing only the file being guarded."
        )

    layers = manifest.get("setup_soldr_layers")
    if isinstance(layers, dict):
        for layer, spec in layers.items():
            if not isinstance(spec, dict):
                continue  # the free-text 'comment' key
            if spec.get("tier") not in vocabulary:
                problems.append(
                    f"R1 setup_soldr_layers[{layer!r}] declares unknown tier "
                    f"{spec.get('tier')!r}"
                )

    entries = manifest.get("entries")
    if not isinstance(entries, list):
        problems.append("R1 manifest must declare a list of 'entries'")
        return problems

    seen: set[str] = set()
    for entry in entries:
        if not isinstance(entry, dict):
            problems.append(f"R1 manifest entry is not an object: {entry!r}")
            continue
        entry_id = str(entry.get("id", ""))
        if not entry_id:
            problems.append(f"R1 manifest entry has no id: {entry!r}")
            continue
        if entry_id in seen:
            problems.append(f"R1 duplicate manifest entry id {entry_id!r}")
        seen.add(entry_id)
        for field in ("workflow", "step", "mechanism", "tier", "rationale"):
            if not str(entry.get(field, "")).strip():
                problems.append(f"R1 entry {entry_id!r} is missing '{field}'")
        tier = entry.get("tier")
        if tier not in vocabulary:
            problems.append(
                f"R1 entry {entry_id!r} declares unknown tier {tier!r}; "
                f"declare it under 'tiers' or 'exceptions' first"
            )
        mechanism = str(entry.get("mechanism", ""))
        if tier == "same-run-transport" and mechanism not in ARTIFACT_MECHANISMS:
            problems.append(
                f"R5 entry {entry_id!r} claims 'same-run-transport' with mechanism "
                f"{mechanism!r}. Transport is run-keyed upload/download-artifact; a "
                "cross-run cache cannot be transport."
            )
        if mechanism in DURABLE_MECHANISMS and tier == "same-run-transport":
            problems.append(
                f"R5 entry {entry_id!r} uses the cross-run mechanism {mechanism!r} "
                "and may not claim 'same-run-transport'."
            )
    return problems


# --------------------------------------------------------------------------
# R2 -- coverage, in both directions
# --------------------------------------------------------------------------


def coverage_problems(manifest: dict, steps: list[PersistedStep]) -> list[str]:
    """Workflows persisting without an entry, and entries that have rotted."""
    problems: list[str] = []
    entries = [e for e in manifest.get("entries") or [] if isinstance(e, dict)]
    covered = {str(e.get("workflow", "")) for e in entries}
    persisting = {step.workflow for step in steps}

    for workflow in sorted(persisting - covered):
        problems.append(
            f"R2 {workflow} persists something but has no entry in the cache "
            "ownership manifest.\n"
            "     Add one naming the step, its tier (cook | zccache-unit | none | "
            "a declared exception) and a one-line rationale. Classification is by "
            "CONTENTS, not by whether GitHub calls the store a cache."
        )
    for workflow in sorted(covered - persisting):
        problems.append(
            f"R2 the manifest has entries for {workflow}, which no longer persists "
            "anything. Remove them -- a manifest that records stores that are gone "
            "cannot be read to find the ones that are real."
        )
    return problems


# --------------------------------------------------------------------------
# R3 -- no test products in a durable store
# --------------------------------------------------------------------------


def banned_product_problems(steps: list[PersistedStep]) -> list[str]:
    """Durable stores in normal workflows that name a linked test product.

    Named `banned_product_problems` rather than `test_product_problems` so
    pytest never mistakes the guard's own helper for a test case.
    """
    problems: list[str] = []
    for step in steps:
        if step.workflow in EXPERIMENT_WORKFLOWS or not step.durable:
            continue
        for label, pattern in TEST_PRODUCT_PATTERNS:
            match = pattern.search(step.text)
            if match is None:
                continue
            problems.append(
                f"R3 {step.describe()} writes {label} into a cross-run reusable "
                f"store (matched {match.group(0)!r}).\n"
                "     Linked test products are tier 3 (soldr#2931): test binaries, "
                "benches, examples built for tests, doctest products, test debug "
                "sidecars and test incremental state may not enter cook, an rlib "
                "cache, a broad target/ cache, or a compiler-cache archive.\n"
                "     If this is a same-run handoff to another job, move it to "
                "upload-artifact/download-artifact and classify it as "
                "'same-run-transport' in ci/cache-ownership.json."
            )
            break
    return problems


# --------------------------------------------------------------------------
# R4 -- no broad target/ restore downstream of cook
# --------------------------------------------------------------------------


def is_broad_target_restore(step: PersistedStep) -> bool:
    """Does this step restore the whole cargo target directory?

    `Swatinem/rust-cache` always does (it takes no path input and owns
    `target/` plus the cargo home). `actions/cache` does when its path names
    the target directory itself rather than a slice of it. setup-soldr's
    `target-cache` in `full` mode does too -- `thin` is the dependency-scoped
    slice and is fine.
    """
    if step.mechanism == "Swatinem/rust-cache":
        return True
    if step.mechanism == "setup-soldr:target-cache":
        return "full" in step.text.lower()
    if step.mechanism != "actions/cache":
        return False
    return any(line.strip() in BROAD_TARGET_PATHS for line in step.text.splitlines())


def cook_ordering_problems(steps: list[PersistedStep]) -> list[str]:
    """Broad `target/` restores that land after a cook restore in the same job."""
    problems: list[str] = []
    jobs: dict[tuple[str, str], list[PersistedStep]] = {}
    for step in steps:
        if step.workflow in EXPERIMENT_WORKFLOWS:
            continue
        jobs.setdefault((step.workflow, step.job), []).append(step)

    for (workflow, job), job_steps in sorted(jobs.items()):
        ordered = sorted(job_steps, key=lambda s: s.index)
        cook = next(
            (
                s
                for s in ordered
                if s.mechanism in {"setup-soldr/cook", "setup-soldr:cook"}
            ),
            None,
        )
        if cook is None:
            continue
        for step in ordered:
            if step.index <= cook.index or not is_broad_target_restore(step):
                continue
            problems.append(
                f"R4 {workflow}: job '{job}' restores a broad target/ snapshot in "
                f"step '{step.name or step.index}' ({step.mechanism}) AFTER the cook "
                f"restore in step '{cook.name or cook.index}'.\n"
                "     The bulk restore overwrites cook's dependency-scoped tree with "
                "a snapshot keyed on a whole-workspace hash -- test binaries and "
                "incremental state included -- so cook bought nothing and the store "
                "now carries what tier 3 forbids (soldr#2931).\n"
                "     Move the broad restore BEFORE the cook step, or narrow it."
            )
    return problems


# --------------------------------------------------------------------------


def load_manifest(path: pathlib.Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def check(manifest_path: pathlib.Path, workflow_dir: pathlib.Path) -> list[str]:
    """Every policy failure, as actionable lines. Empty means the tree is clean."""
    try:
        manifest = load_manifest(manifest_path)
    except (OSError, json.JSONDecodeError) as error:
        return [f"R1 cannot read {manifest_path}: {error}"]
    if not isinstance(manifest, dict):
        return [f"R1 {manifest_path} must contain a JSON object"]

    problems = manifest_problems(manifest)
    steps = collect_steps(workflow_dir)
    problems += coverage_problems(manifest, steps)
    problems += banned_product_problems(steps)
    problems += cook_ordering_problems(steps)
    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest",
        type=pathlib.Path,
        default=MANIFEST,
        help="cache ownership manifest (default: ci/cache-ownership.json)",
    )
    parser.add_argument(
        "--workflow-dir",
        type=pathlib.Path,
        default=WORKFLOW_DIR,
        help="directory of workflow YAML to scan (default: .github/workflows)",
    )
    args = parser.parse_args(argv)

    problems = check(args.manifest, args.workflow_dir)
    if problems:
        print("error: cache ownership policy violated (soldr#2931 / soldr#2937):")
        for problem in problems:
            print(f"  {problem}")
        print()
        print(
            "Cache admission follows the stability of an artifact's identity key\n"
            "relative to its size. Linked test products have the least stable key\n"
            "in the build and the largest size, so they are never cacheable.\n"
            "See ci/cache-ownership.json for the tier definitions and the list of\n"
            "named exceptions."
        )
        return 1

    print(
        "check_cache_ownership: every persisted store is classified; no test "
        "products in cross-run stores."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
