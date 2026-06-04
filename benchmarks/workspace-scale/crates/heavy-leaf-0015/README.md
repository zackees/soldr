# heavy-leaf-0015

WebAssembly tooling subgraph (pure-Rust): `wasmi` (pure-Rust
interpreter), `wasmparser`, `wat` (text → binary assembler),
`wasmprinter` (binary → text), `wasm-encoder` (builder), `wast`
(full text-format parser), `walrus` (binary edit library),
`wasm-smith` (test-shape generator), `wasm-metadata`, `leb128`.
Distinct from the prior fourteen leaves' subgraphs. No `cc-rs`
build scripts (wasmtime / cranelift-with-cc-rs avoided to stay
pure-Rust). See parent `../README.md` and soldr#648.
