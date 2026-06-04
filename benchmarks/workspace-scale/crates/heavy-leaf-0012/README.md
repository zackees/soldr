# heavy-leaf-0012

TUI + syntax-highlighting subgraph (pure-Rust): `ratatui` (with
widgets + crossterm backend), `crossterm`, `tui-textarea`,
`tui-tree-widget`, `syntect` (fancy-regex variant — `onig`
avoided to stay pure-Rust), `syntect-tui`, `termion` (alt
backend), `console`, `supports-color`, `supports-hyperlinks`,
`supports-unicode`, `vt100`. Heavy regex + finite-state graphs
distinct from the prior eleven leaves' subgraphs. No `cc-rs`
build scripts. See parent `../README.md` and soldr#648.
