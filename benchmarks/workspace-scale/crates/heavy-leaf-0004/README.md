# heavy-leaf-0004

Math / numerical / sampling subgraph: `ndarray` (rayon + serde),
`nalgebra`, `statrs`, `rand` + `rand_distr` + `rand_chacha`,
`num-traits`, `num-complex`, `itertools`, `smallvec`. Pure-Rust
deps — no `cc-rs` build scripts, so the leaf stays portable across
all four bench-supported runner OSes. See parent README + soldr#648.
