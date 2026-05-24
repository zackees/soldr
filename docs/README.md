# soldr documentation

External-facing docs grouped by topic. `API.md` is the source-of-truth
contract for what soldr exposes to consumers; the others document
operational and integration details.

## Files

- `API.md` / `API_BOUNDARY.md` — public soldr CLI contract and the
  invariants that bound it.
- `CI_CACHE.md` / `CI_CACHE_PHASE1.md` — usage guide and historical
  rollout notes for `zackees/setup-soldr@v0` in external CI.
- `NPM_PUBLISHING.md`, `PYPI_TRUSTED_PUBLISHING.md` — release plumbing.
- `RELEASE_*.md` — release checklists and verification flow.
- `RUSTUP_REPLACEMENT_ANALYSIS.md` — design discussion for the rustup
  replacement path.
- `SETUP_SOLDR_EXPORTER.md` / `SETUP_SOLDR_PUBLIC_ACTION.md` —
  exporter design and the public action contract.
- `TARGET_GC_PROPOSAL.md`, `THIN_TARGET_CACHE_PRUNING.md` — Rust
  artifact cache / target-dir GC design notes.
- `TRUST_BOUNDARIES.md` — trust-mode semantics.
