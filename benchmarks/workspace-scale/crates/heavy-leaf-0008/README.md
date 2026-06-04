# heavy-leaf-0008

Embedded-storage + concurrent collections subgraph (pure-Rust):
`redb` (embedded KV), `moka` (TinyLFU cache), `lru`, `schnellru`,
`dashmap` + `scc` (concurrent maps), `indexmap` + `slotmap` +
`slab` (indexed collections), `petgraph` (graphs), `rangemap`,
`fst` (finite state transducer), `growable-bloom-filter`,
`ahash` + `foldhash` + `hashbrown`. Distinct from the prior seven
leaves' web / http-client / cli / math / parser / crypto /
serialization subgraphs. No `cc-rs` build scripts (sled /
rocksdb avoided to stay pure-Rust). See parent `../README.md`
and soldr#648.
