# Vendoring zccache into `_vender/` for fast iteration

> [!NOTE]
> No zccache or running-process vendor is active. soldr#2765 retired both
> submodules; production builds resolve exact published crate versions. This
> document applies only if a future, explicitly time-bounded vendor is started.

> **Audience:** soldr contributors investigating a bug that spans
> soldr and zccache. The canonical example: the cold-build IPC
> regression tracked in soldr#981 — fixing it requires changes
> *inside* `zccache::daemon::server` and `zccache::embedded`, which
> we cannot iterate on cleanly via the `git = "..."` Cargo dep.
>
> **Companion doc:** [`docs/architecture/vendored-hotfix-workflow.md`](../docs/architecture/vendored-hotfix-workflow.md)
> — that doc explains the loop from zccache's side ("how a host
> validates a candidate fix"). This one explains it from soldr's
> side ("how the soldr repo organizes the vendored copy + when it
> ends + how it interacts with soldr's own version bumps").

## When to vendor

Vendor zccache into `_vender/zccache/` **only** when the work
satisfies all three rules below:

1. **Cross-repo investigation** — the bug or perf issue is in
   zccache code that is reached *only* through soldr's embedded
   integration. A fix that doesn't need soldr context belongs as
   a direct PR against `zackees/zccache`.
2. **The git-rev iteration loop is too slow** — measuring each
   candidate fix would require an upstream PR, merge, release, and
   bump on the soldr side. For a single bug that's fine. For a
   sequence of ~5 iterations spread over a few days, the loop is
   intolerable.
3. **There is a named meta issue with a closure date** — vendored
   work without a closure date rots into a long-lived fork. The
   strategy below depends on the meta issue itself enforcing the
   deadline.

If any rule isn't met, do not vendor — open a direct PR against
zccache.

## What lives in `_vender/zccache/`

A **complete, unmodified-from-upstream snapshot** of the
`zackees/zccache` repository at the `_vender/` checkpoint commit.
Not a subtree merge, not a submodule — a flat copy committed to
soldr's history. Soldr ships from soldr's history; the vendored
zccache compiles from the path dep.

```
_vender/
└── zccache/                          # the entire zccache repo, root-flat
    ├── README.md
    ├── Cargo.toml                    # zccache workspace root
    ├── crates/
    │   ├── zccache/                  # the library crate soldr-cli depends on
    │   ├── zccache-cli/
    │   ├── zccache-watcher/
    │   └── zccache-fingerprint/
    ├── docs/
    │   ├── architecture/
    │   │   ├── embedded-service.md
    │   │   ├── vendored-hotfix-workflow.md
    │   │   └── ...
    │   └── ...
    ├── .vendor-state                 # NEW — see "Vendor metadata" below
    └── ...
```

Critical: `_vender/zccache/` is the **whole** zccache repo, not just
the one crate soldr consumes. Vendoring less makes upstreaming work
back impossible because diffs across crate boundaries are
unrepresentable.

## Vendor metadata: `_vender/zccache/.vendor-state`

A single, hand-maintained file that records the vendor's provenance
and deadline:

```toml
# Provenance — what upstream commit this snapshot was taken from.
upstream_repo = "https://github.com/zackees/zccache.git"
upstream_branch = "main"
upstream_sha = "fde4b916f24d9fe1cd00aeabe6581e4972f32068"
upstream_tag_at_sync = "1.12.11"   # nearest released tag at sync time

# Cadence — when this vendor was created and when it MUST end.
synced_at = "2026-06-26T19:00:00Z"
deadline = "2026-07-10T00:00:00Z"   # 2 weeks from sync. Hard-cap.

# Driver — the soldr issue that brought the vendor into existence.
# This issue closes when the vendor is removed (per the contract below).
driving_issue = "https://github.com/zackees/soldr/issues/981"

# Local deltas — every commit inside _vender/zccache/ that diverges
# from the upstream snapshot. Each entry MUST cite the matching
# upstream PR once that PR is open. Until the upstream PR is open
# the `upstream_pr` field stays as `null` and the contract
# considers the change "in flight, not yet back-ported."
[[deltas]]
soldr_commit = "<soldr-commit-sha>"
summary = "embedded: streaming Response::Compile chunks for stdout/stderr"
upstream_pr = null

[[deltas]]
soldr_commit = "<soldr-commit-sha>"
summary = "embedded: eliminate per-call redb-open in ZccacheService::compile"
upstream_pr = "https://github.com/zackees/zccache/pull/943"
```

The `.vendor-state` file is the **only** durable record of what
diverges and when it must end. The contract enforces a manual
audit: any soldr PR that lands changes inside `_vender/zccache/`
must update `.vendor-state` in the same commit, adding a `[[deltas]]`
entry. Skipping this is a review-rejection criterion.

## Soldr's Cargo.toml: path dep + comment

In `crates/soldr-cli/Cargo.toml`:

```toml
# Vendored for the duration of soldr#981 (deadline 2026-07-10).
# When the upstream fixes land + release, this MUST switch back to
# the released zccache (crates.io or git rev pin) and `_vender/zccache/`
# MUST be deleted. See docs/VENDORING.md for the contract and
# .vendor-state for the upstream PR status of each local delta.
zccache = { path = "../../_vender/zccache/crates/zccache" }
```

CI checks the comment is still there (a single grep) so future
contributors can't quietly remove the deadline marker.

## The contract

The vendor exists under **three** non-negotiable rules:

1. **No local-only fixes.** Every change inside `_vender/zccache/`
   must be upstreamable. The `[[deltas]]` entry in `.vendor-state`
   must include the `upstream_pr` URL within **one week** of the
   change landing in soldr. After one week with `upstream_pr = null`,
   a CI gate fails the soldr build.

2. **No long-lived vendor.** The `deadline` field is a hard cap.
   Two weeks from the `synced_at` date is the default. By the
   deadline, **every** local delta MUST be merged upstream and
   released, AND the soldr side MUST remove `_vender/zccache/` and
   restore the released-version pin. Bumping the deadline requires
   an explicit comment on the driving issue stating *why* the
   original target slipped.

3. **No drift.** No upstream change lands in `_vender/zccache/`
   unless it is a direct cherry-pick (or rebase) of an
   already-merged upstream commit. We never modify upstream code
   except via the `[[deltas]]` mechanism, and we never silently
   absorb other upstream changes. If we need a newer upstream main,
   we re-sync the whole snapshot and re-apply the deltas — recorded
   as a new `synced_at` + a fresh `deadline`.

Violating rule 1 or 2 escalates to closing the meta issue and
removing the vendor — even if the fix isn't merged upstream yet.
The rules exist to prevent the exact failure mode the
`vendored-hotfix-workflow` doc warns about: a host vendoring
upstream code, never upstreaming the fix, and ending up with an
untrackable fork.

## The day-to-day workflow

### Initial vendoring (one-time per investigation)

```bash
# From the soldr repo root, ensure we're on a feature branch.
git switch -c perf/cold-build-roadmap-980-vendor

# 1. Snapshot upstream zccache at the current pin.
UPSTREAM_SHA=fde4b916f24d9fe1cd00aeabe6581e4972f32068
mkdir -p _vender
git -C _vender clone --depth=1 https://github.com/zackees/zccache.git zccache
git -C _vender/zccache fetch --depth=1 origin "$UPSTREAM_SHA"
git -C _vender/zccache checkout "$UPSTREAM_SHA"
rm -rf _vender/zccache/.git   # we don't carry the upstream git history

# 2. Write .vendor-state with the synced_at + deadline + driving_issue.
cat > _vender/zccache/.vendor-state <<EOF
upstream_repo = "https://github.com/zackees/zccache.git"
upstream_branch = "main"
upstream_sha = "$UPSTREAM_SHA"
upstream_tag_at_sync = "1.12.11"
synced_at = "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
deadline = "$(date -u -d '+2 weeks' +%Y-%m-%dT00:00:00Z)"
driving_issue = "https://github.com/zackees/soldr/issues/981"
EOF

# 3. Switch Cargo.toml's zccache dep to path =.
# (Replace the `git = "..."` line in crates/soldr-cli/Cargo.toml.)
# Run `cargo update` to materialize the new lockfile entry.

# 4. Single commit that introduces the vendor.
git add _vender/ crates/soldr-cli/Cargo.toml Cargo.lock docs/VENDORING.md
git commit -m "chore(vendor): start _vender/zccache/ for soldr#981 (deadline 2026-07-10)"
```

### Iterating on a fix

Each candidate fix is a normal soldr commit that touches files
*inside* `_vender/zccache/`:

```bash
# Edit the upstream code in place — these are soldr-side changes
# for now; they become an upstream PR later.
$EDITOR _vender/zccache/crates/zccache-daemon-core/src/embedded.rs

# Run the soldr perf harness against the change.
docker build -f ci/docker/profile/Dockerfile.perf-linux -t soldr-profile-linux .
STAMP=$(date -u +%Y-%m-%d-%H%M)
mkdir -p ".codex-artifacts/soldr-vendor-perf-${STAMP}"
MSYS_NO_PATHCONV=1 docker run --rm --privileged \
    --cap-add=SYS_ADMIN --cap-add=SYS_PTRACE \
    -e SOLDR_PROFILE_SCENARIOS="cold warm" \
    -v "$(pwd)":/work \
    -v "$(pwd)/.codex-artifacts/soldr-vendor-perf-${STAMP}":/out \
    soldr-profile-linux

# If the run improves cold/warm meaningfully, commit + update .vendor-state.
git add _vender/zccache/ _vender/zccache/.vendor-state
git commit -m "embedded: <fix summary> (refs soldr#981)"
```

After a soldr commit, **always** add a `[[deltas]]` block in
`.vendor-state` with `upstream_pr = null` until the matching
upstream PR is open. This is the audit trail.

### Upstreaming a fix

When a vendored fix is proven by the soldr perf harness:

```bash
# 1. Cherry-pick the soldr commit onto a fresh branch in the
#    upstream zccache checkout (not the vendored copy).
cd ~/dev/zccache  # the regular zccache checkout
git switch -c fix/embedded-streaming-compile-response

# Manually apply the changes from the soldr commit. (You can use
# `git -C ~/dev/soldr show <sha> -- _vender/zccache/...` and pipe
# the diff through `patch -p4` after stripping the path prefix.)
git apply --3way --include='_vender/zccache/*' \
    <(git -C ~/dev/soldr show <soldr-commit-sha>) -p4 \
    --directory=$(pwd)/

# 2. Open the upstream PR.
gh pr create --repo zackees/zccache --title "embedded: <fix>" --body "..."

# 3. Back in the soldr checkout, update .vendor-state to point at
#    the upstream PR URL.
$EDITOR _vender/zccache/.vendor-state   # set upstream_pr = "https://..."
git commit -am "chore(vendor): link upstream PR for streaming-compile fix"
```

### Ending the vendor

When all `[[deltas]]` upstream PRs are **merged AND released** in a
new zccache version:

```bash
# 1. Record the released zccache crate version that contains every delta.
#    zccache is an embedded API dependency; there is no managed binary,
#    standalone zccache release asset, or embedded download manifest to update.

# 2. Switch zccache back from path = to the released version.
# Replace the `zccache = { path = "../../_vender/zccache/..." }`
# line with the released form:
#   zccache = { version = "=<released-version>" }

# 3. Delete the vendor.
git rm -r _vender/zccache/
# Keep _vender/ if anything else is vendored; otherwise rmdir it.

# 4. Single commit that closes the vendor + the driving issue.
git add -A
git commit -m "chore(vendor): retire _vender/zccache/ — soldr#981 closed by zccache <version> (#NNN)"

# 5. PR description must say "closes soldr#981" so the driving
#    issue auto-closes on merge.
```

## How this interacts with soldr's own version bumps

Soldr's `Cargo.toml` carries `[workspace.package] version = "X.Y.Z"`.
When the vendor is active, soldr continues to release as normal —
the vendor is invisible to consumers because the soldr binary is
shipped as a normal cargo build that includes the vendored zccache
statically.

Two interactions matter:

1. **Soldr release while vendor is active**: legal but adds risk.
   The release tarball includes the vendored zccache code, which
   has unreleased fixes. The release notes MUST cite the driving
   issue + the upstream PRs that are pending. After the deadline,
   if the vendor is still active when a soldr release is cut, the
   release notes must explicitly call out "this release ships
   non-upstreamed zccache code; track soldr#981."

2. **Vendored zccache pin/version bump while vendor is active**: legal
   and substantive. The vendored source is the embedded library implementation
   linked into `soldr-daemon`, and its version is recorded in `manifest.json`.
   There is no second executable or managed-binary version to synchronize.

When the vendor ends and we restore the released git/crates.io pin,
only the library pin moves — there is no separate managed-binary
version to bump alongside it (soldr#1368).

## CI enforcement

Three lightweight CI checks gate the vendor's discipline:

1. **`_vender/zccache/.vendor-state` exists when `Cargo.toml` says
   `zccache = { path = "_vender/..." }`** — catches the "forgot the
   metadata" mistake.

2. **`deadline` is in the future** — a CI step that parses the TOML
   and fails the build if `deadline < now()`. The vendor is meant
   to end; CI helps enforce it.

3. **Every `[[deltas]]` entry older than 7 days has a non-null
   `upstream_pr`** — a CI step that walks the deltas and fails if
   any soldr-side change hasn't been turned into an upstream PR
   within a week.

A reference implementation lives in
`.github/scripts/verify_vendor_state.py` and runs on every PR
that touches `_vender/`.

## What this strategy is NOT

- It is **not** a permanent fork. Two weeks is the default deadline
  and bumping it requires a justification on the driving issue.
- It is **not** a workaround for upstream review velocity. If the
  zccache maintainers are slow to merge, the right response is a
  social conversation, not silent divergence.
- It is **not** a way to ship soldr-specific features into zccache.
  Anything that lands in `_vender/zccache/` must be host-agnostic,
  upstream-able to a generic embedded-service consumer. Soldr-only
  behaviour belongs on the soldr side.

## Cross-references

- soldr#981 — the cold-build IPC regression the vendor exists to fix
- soldr#977 — the embedded-service adoption tracker
- soldr#980 — the cold-build performance roadmap
- [`docs/architecture/vendored-hotfix-workflow.md`](architecture/vendored-hotfix-workflow.md)
  in the zccache repo — the upstream-facing companion contract
- [`CLAUDE.md`](../CLAUDE.md) §"Bumping managed_zccache_version" —
  the lockstep version-bump procedure that runs when the vendor
  ends
