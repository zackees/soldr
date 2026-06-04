# heavy-leaf-0013

Pure-Rust git (gitoxide) + filesystem tooling subgraph: `gix`
facade pulls ~40 sub-crates (gix-config, gix-hash, gix-object,
gix-odb, gix-pack, gix-protocol, gix-ref, gix-revwalk,
gix-status, gix-worktree, gix-traverse, gix-transport,
gix-index, gix-mailmap, gix-features, gix-attributes,
gix-pathspec, gix-glob, …). Layered on top: `ignore` (gitignore
semantics), `walkdir`, `jwalk` (parallel walk), `globset`,
`fs-err`, `tempfile`, `notify` (FS watcher), `dunce` (Windows
path canonicalization), `semver`, `git-conventional`. Distinct
from the prior twelve leaves' subgraphs. No `cc-rs` build scripts
(libgit2 / git2 avoided to stay pure-Rust). See parent
`../README.md` and soldr#648.
