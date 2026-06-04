# heavy-leaf-0002

Sibling leaf to `heavy-leaf-0001/`. Pulls in `reqwest` (with
`rustls-tls`) instead of `axum`, so the transitive dep subgraph
overlaps but isn't identical to leaf 0001 — that grows
`target/release` per leaf without doubling the cargo registry
baseline. See the parent `../README.md` for the workspace-scale
strategy and soldr#648.
