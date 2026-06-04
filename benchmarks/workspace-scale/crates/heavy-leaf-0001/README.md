# heavy-leaf-0001

Seed leaf crate for `benchmarks/workspace-scale/`.

This crate's source is deliberately minimal. All the compilation
work that grows `target/` lives in the dependency graph in
`Cargo.toml` — tokio (full), axum, tower-http, hyper, tonic, prost,
serde, regex, tracing, and friends. A release build of just this
one leaf produces ~80-100 transitive rustc invocations.

Subsequent PRs will add more leaves (`heavy-leaf-0002` and onward)
to the parent workspace's `members[]` list until `target/release`
exceeds the 10 GiB GitHub Actions cache size limit, at which point
`Swatinem/rust-cache`'s `actions/cache` save step stops being able
to persist the workspace. soldr's content-addressed cache stays
well under the cap because it doesn't carry the full `target/`,
only the artifacts each compile actually produces. This is the
fbuild-scale workspace-scale scenario tracked under
[soldr#648](https://github.com/zackees/soldr/issues/648).

## Why the deliberate minimal source

Per the loop's top-level constraint ("don't blow up the cache
budget with few builds"), the fixture has to grow incrementally.
Each leaf adds ~50-100 MiB of cached crate downloads to the cargo
registry baseline; we want to spread that across PRs rather than
land 100 leaves in one commit and force every reviewer to wait
for the full registry warm-up.
