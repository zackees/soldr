# Manifest-branch Git LFS migration runbook

This document is the **owner-runnable** procedure for converting vendored
binary assets on the `manifest` branch (today: regular blobs) into
Git-LFS-tracked objects. **No part of this conversion has been performed
yet.** Everything here is prep work: tooling, snippets, and the math that
the owner needs to review before flipping a switch that affects
`zackees/soldr` billing in irreversible ways.

Refs: #853 (meta), #872 (this sub-issue), #862 (Apple SDK vendor), #864
(LLVM vendor), #866 (manifest-first fetch), #868 (`asset-index.json`
publisher), #871 (manifest schema v6).

---

## 1. Intent

The `manifest` branch is a long-lived orphan branch whose tree mirrors a
public release-asset catalogue. As of #871 it carries vendored payloads
under `deps/<host-triple>/<tool>/<version>/<asset>` so that soldr
bootstrap is durable against upstream takedowns or asset renames at
`ziglang.org`, GitHub-Releases mirrors, etc.

Those payloads are presently stored as **regular git blobs**. That works
today (largest single asset is on the order of ~50 MiB) but:

- Cloning the `manifest` branch already costs the full sum of vendored
  bytes on every fresh CI runner (even when only one asset is needed).
- The next vendored tool — LLVM 21.1.5 full distribution, multi-arch
  MSVC CRT — pushes the branch toward hundreds of MiB.
- Every `manifest`-branch rewrite (we do them whenever the index format
  bumps; see #871) doubles the pack-file footprint until `git gc`.

Switching the vendored files to LFS means: pointer files in the working
tree, real bytes in `https://github.com/zackees/soldr.git/info/lfs/...`,
fetched on demand only when a workflow actually wants the asset.

The win is durability + bandwidth proportional to actual use.

---

## 2. One-time owner actions (manual, sequential)

These steps require either repo-admin scope on `zackees/soldr` or a
local clone of the `manifest` branch. The order matters. **Do not skip
the dry-run verification in step 2.4.**

### 2.1 Enable LFS for the repo

1. Visit <https://github.com/zackees/soldr/settings>.
2. Scroll to **Features** > **Git LFS** and toggle it on. (If the
   setting is hidden you may need to be repo-admin, not just a
   collaborator.)
3. Visit <https://github.com/account/billing> to confirm the LFS
   storage / bandwidth plan attached to the account. The default free
   tier is **1 GB storage + 1 GB bandwidth / month** — see the math in
   section 3 before continuing.

### 2.2 Install Git LFS locally

```bash
# macOS
brew install git-lfs
# Debian / Ubuntu
sudo apt install git-lfs
# Windows (Git for Windows ships it; otherwise)
winget install GitHub.GitLFS

# Then, once per machine:
git lfs install
```

`git lfs install` writes filter hooks into `~/.gitconfig`. It is a
machine-local op; nothing has been written to the repo yet.

### 2.3 Convert existing assets on the `manifest` branch

From a clean checkout of the `manifest` branch:

```bash
git checkout manifest
git status   # must be clean

bash .github/scripts/lfs_migrate_manifest_branch.sh
```

The script wraps:

```bash
git lfs migrate import \
    --include="deps/**/*.tar.zst,deps/**/*.tar.xz,deps/**/*.zip,deps/**/*.tar.gz" \
    --everything
```

`--everything` rewrites every reachable commit on the branch. The
script refuses to run if the working tree is dirty or if LFS is not
installed; it prints a before/after byte-count summary and **does not
push**.

### 2.4 Review the rewrite

```bash
git log --oneline manifest -20
git lfs ls-files | head
git cat-file -p HEAD:deps/<some-asset>   # should show "version https://git-lfs.github.com/spec/v1 ..."
du -sh .git
```

If anything looks wrong, the rewrite lives entirely in the local clone
— delete the clone and start over. Nothing has reached `origin` yet.

### 2.5 Push

```bash
# Push LFS objects first so the pointer-only ref-update can resolve them.
git lfs push --all origin manifest

# Then update the ref. Force is required: history was rewritten.
git push --force-with-lease origin manifest
```

**Why force-push is acceptable on `manifest` specifically:** this branch
is a vendored-assets index, not a source-history branch. It is
rewritten on every schema bump (most recently #871 → v6) and has no
external consumers that depend on stable commit SHAs. Consumers
(`raw.githubusercontent.com/zackees/soldr/manifest/...` URLs in
workflow scripts) only care about path-level content.

### 2.6 Add the `.gitattributes` declaration

The snippet at `.github/snippets/manifest-branch.gitattributes` should
be copied into the root of the `manifest` branch as `.gitattributes`
**after** the migration finishes, so future commits to `deps/` continue
to land in LFS:

```bash
git checkout manifest
cp .github/snippets/manifest-branch.gitattributes .gitattributes
git add .gitattributes
git commit -m "manifest: declare LFS tracking for deps/ payloads"
git push origin manifest
```

(Step 2.3 already retroactively put existing files into LFS; this step
just makes sure the next vendored asset commit obeys the same rule
without anyone needing to remember.)

---

## 3. Bandwidth math vs GitHub LFS quota

GitHub's default LFS quota on a personal account: **1 GB storage,
1 GB bandwidth / month**, with $5/month "data packs" buying +50 GB of
each.

### 3.1 Storage estimate

Approximate sizes of currently-vendored payloads (from `deps/` on the
post-#864 manifest branch):

| Asset                                              | ~size  |
| -------------------------------------------------- | ------ |
| zig 0.13.0 (linux-x86_64)                          | 45 MiB |
| zig 0.13.0 (macos-aarch64)                         | 45 MiB |
| zig 0.13.0 (windows-x86_64)                        | 75 MiB |
| LLVM 21.1.5 minimal (linux-x86_64)                 | 60 MiB |
| LLVM 21.1.5 minimal (macos-aarch64)                | 55 MiB |
| LLVM 21.1.5 minimal (windows-x86_64)               | 70 MiB |
| MacOSX 11.3 SDK (.tar.xz)                          | 90 MiB |
| MSVC CRT bundle                                    | 30 MiB |
| **Subtotal initial migration**                     | **~470 MiB** |

LFS deduplicates by content hash, so re-pushing identical bytes is
free. The 470 MiB initial migration fits comfortably inside the 1 GB
default storage tier.

### 3.2 Bandwidth estimate

Bandwidth is the load-bearing dimension. Estimate inputs:

- `cross-compile-all-targets.yml` runs **~7 target lanes** per push to
  `main` and per release.
- Each lane resolves **1 zig + 1 LLVM + (on macOS) 1 SDK**. Average
  per-lane LFS fetch: ~150 MiB.
- Pushes to `main`: ~10/week ≈ **40/month**. Releases: ~4/month.
- Other consumers (setup-soldr Action installs, manual reproductions,
  dependabot reruns): rough fudge factor of **+50%**.

Per-month LFS bandwidth:

```
(40 main pushes + 4 releases) × 7 lanes × 150 MiB × 1.5 fudge
  ≈ 44 × 7 × 150 × 1.5 MiB
  ≈ 69,300 MiB
  ≈ 67.7 GiB / month
```

**This blows past the 1 GB free tier by ~70x and past one data-pack
(+50 GB) by ~1.3x.** Conclusion: enabling LFS naively without a CDN
fronting it will incur a real monthly LFS-bandwidth bill (~$10/month
of data packs at current volume, scaling with workflow growth).

### 3.3 Mitigations to apply before enabling LFS

1. **Cache the LFS payloads on the runner.** The
   `actions/cache@v4` keyed by asset SHA-256 collapses identical
   fetches across reruns. Soldr's `~/.soldr/cache/` already does this
   at the soldr level; the missing piece is making sure the
   `manifest`-branch raw-URL fetch path also passes through the cache
   rather than re-downloading every job.
2. **Fall back to S3 if bandwidth becomes load-bearing.** Document the
   `SOLDR_MANIFEST_MIRROR_BASE_URL` env-var hook so production
   deployments can point at an S3/CloudFront mirror that holds a copy
   of the same LFS objects. (S3 egress to GitHub Actions runners is
   ~$0.09/GB; at 70 GiB/month that's ~$6.30/month, comparable to a
   GitHub data pack but with no quota cliff.)
3. **Restrict LFS to truly-large assets.** The `.gitattributes` snippet
   currently tracks `deps/**/*.tar.zst,tar.xz,zip,tar.gz`. If a future
   vendored asset is small (< 1 MiB), commit it as a regular blob and
   exclude its pattern.

### 3.4 S3 fallback path (documented but not implemented)

Owner action sequence if GitHub LFS bandwidth becomes a problem:

1. Create an S3 bucket `soldr-manifest-mirror` (or equivalent).
2. Mirror the LFS objects with `git lfs fetch --all` followed by an
   `s3 sync` of `.git/lfs/objects/`.
3. Set an env var consumed by soldr's fetch path:
   ```bash
   export SOLDR_MANIFEST_MIRROR_BASE_URL="https://soldr-manifest-mirror.s3.amazonaws.com"
   ```
   The fetch path tries the mirror first, falls back to
   `raw.githubusercontent.com`. (This env-var plumbing is **not yet
   wired** — it is a separate follow-up that would be filed if the
   fallback becomes necessary.)

---

## 4. Rollback plan

If LFS bandwidth gets exhausted mid-month and CI starts failing with
`This repository is over its data quota`, here is how to back out:

1. **Buy a data pack** (`https://github.com/settings/billing` → **Git
   LFS Data**, +50 GB / $5). This unblocks CI immediately.
2. If the owner does **not** want to pay long-term, re-bake the
   vendored assets as regular blobs:
   ```bash
   git checkout manifest
   git lfs migrate export \
       --include="deps/**/*.tar.zst,deps/**/*.tar.xz,deps/**/*.zip,deps/**/*.tar.gz" \
       --everything
   rm .gitattributes
   git add .gitattributes
   git commit -m "manifest: revert LFS, bytes back to regular blobs"
   git push --force-with-lease origin manifest
   ```
3. After rollback, disable LFS in repo settings to stop the
   `git-lfs.github.com` quota meter from running (it bills on
   bandwidth, not just push count).

The rollback is symmetric to the migration: `migrate import` ↔
`migrate export`. The same force-push justification applies.

---

## 5. Why this PR does not flip the switch

This PR ships:

- `docs/MANIFEST_LFS_MIGRATION.md` (this file).
- `.github/scripts/lfs_migrate_manifest_branch.sh` (the runner that
  performs step 2.3 locally and stops before pushing).
- `.github/snippets/manifest-branch.gitattributes` (the file the owner
  copies into the `manifest` branch in step 2.6).
- `tests/test_lfs_migrate_manifest_branch.py` (smoke tests for the
  migrator: arg-parsing + dry-run pre-flight, no real git ops).

It does **not**:

- Run `git lfs install` against `zackees/soldr`.
- Push anything to the `manifest` branch.
- Touch repo settings.

That separation is deliberate: enabling LFS on a public repo affects
billing irreversibly at scale. The owner reviews this runbook, weighs
the bandwidth math in section 3, then runs the steps in section 2
themselves. Issue #872 stays **open** until that handoff completes.
