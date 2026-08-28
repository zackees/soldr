//! soldr#941 — parity test that asserts
//! `crate::core::canonical_targets::CANONICAL_TARGETS` matches the
//! `[workspace.metadata.soldr].targets` block in the root
//! `Cargo.toml` byte-for-byte. Drift between the two would
//! silently produce different `--target all` behaviour depending on
//! whether the workspace metadata is reachable.
//!
//! Standalone test binary so a workspace `Cargo.toml` parse failure
//! (corrupt TOML, missing file in a container build) only fails
//! THIS file, not the rest of the integration tests.

use crate::common;

use soldr_cli::core::CANONICAL_TARGETS;

#[test]
fn canonical_const_matches_workspace_metadata() {
    let workspace_toml = common::workspace_root().join("Cargo.toml");
    assert!(
        workspace_toml.is_file(),
        "expected workspace Cargo.toml at {workspace_toml:?}"
    );

    let text = std::fs::read_to_string(&workspace_toml).expect("read root Cargo.toml");
    let parsed: toml::Value = toml::from_str(&text).expect("parse root Cargo.toml");
    let metadata_targets = parsed
        .get("workspace")
        .and_then(|v| v.get("metadata"))
        .and_then(|v| v.get("soldr"))
        .and_then(|v| v.get("targets"))
        .and_then(|v| v.as_array())
        .expect("[workspace.metadata.soldr].targets must be present in root Cargo.toml");

    let workspace_list: Vec<&str> = metadata_targets
        .iter()
        .map(|v| v.as_str().expect("each target is a string"))
        .collect();

    assert_eq!(
        workspace_list.len(),
        CANONICAL_TARGETS.len(),
        "length mismatch — workspace has {} entries, const has {}",
        workspace_list.len(),
        CANONICAL_TARGETS.len(),
    );

    // Order must match too. The const is documented as
    // deterministic-order; consumers (docker-cross-all build.sh
    // generators, soldr targets shorthand) rely on the order.
    for (i, (workspace_triple, const_triple)) in workspace_list
        .iter()
        .zip(CANONICAL_TARGETS.iter())
        .enumerate()
    {
        assert_eq!(
            workspace_triple, const_triple,
            "drift at index {i}: workspace={workspace_triple}, const={const_triple}",
        );
    }
}
