# workspace-scale benchmark fixture

Closes #648 (skeleton).

A multi-crate workspace fixture sized so that `cargo build --release`
produces a `target/` large enough to exceed GitHub Actions' per-repo
**10 GiB cache size limit**. At that scale `Swatinem/rust-cache`'s
`actions/cache` save step fails (or, depending on whether the repo
has prior smaller saves, evicts) — measuring it directly is the
only way to expose soldr's structural advantage in the published
benchmark, since the existing `soldr-cli` fixture's `target/` is
small enough to fit comfortably (~600-800 MiB) and both backends
restore cleanly.

## Status: SKELETON

This commit lays out the directory + the `[workspace]` `Cargo.toml`
shell so a follow-up can populate it without re-litigating where it
lives or how it integrates with `benchmark.toml`. The actual crate
graph (80-100 dependent crates, each pulling in a different heavy
dep) lands in a subsequent PR — building 80 crates here without
their downstream wiring would already be ~600 MiB of registry
downloads on every soldr CI job, which violates the project's "do
not blow up the cache budget with few builds" constraint from the
top of the loop.

## Design constraints

- After the cold-build, `target/release` must be **≥ 12 GiB** to
  cross the 10 GiB cache limit comfortably.
- Per-build registry footprint (the part that lands in
  `$CARGO_HOME/registry`) should stay under ~3 GiB so the
  cargo baseline doesn't dwarf the per-fixture delta.
- All deps must be available on crates.io with permissive licences.
- No build scripts that hit the network or require system libs other
  than what GHA runners already provide.
- The workspace must build on all four bench-supported runner OSes
  (Linux x64/aarch64, macOS, Windows) for the cell to be valid.

## Hooks into the bench harness

When the crate tree is populated, `benchmark.toml` grows a
`[[fixtures]]` entry pointing here and the workflow Python loop
becomes a `for fixture in fixtures` over the existing
profile × mutation matrix. The current single-fixture shape is the
explicit thing that needs to change next, not the cell structure
itself — `_backend_cache_roots` already takes the backend identity
and is agnostic to which workspace it's measuring.
