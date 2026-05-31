# soldr-cli integration tests

End-to-end / CLI-level integration tests for the `soldr` binary. Each `cli_*.rs` file exercises one CLI surface (cache, cook, install-zccache, wrappers, doctor, etc.) by spawning the built binary with `CARGO_BIN_EXE_soldr` and asserting on stdout/json.
