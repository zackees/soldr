#![cfg(feature = "client")]

use std::path::{Path, PathBuf};

const REQUIRED_V1_DOCS: &[&str] = &[
    "docs/v1-architecture-overview.md",
    "docs/v1-frozen-commitments.md",
    "docs/v1-pipe-naming.md",
    "docs/v1-platform-behavior.md",
    "docs/v1-security-model.md",
    "docs/v1-wire-envelope.md",
    "docs/v1-cache-manifest.md",
    "docs/v1-service-definition.md",
    "docs/v1-lifecycle-events.md",
    "docs/v1-consumer-adoption-dashboard.md",
    "docs/consumer-adoption-clud.md",
    "docs/consumer-adoption-zccache.md",
    "docs/consumer-adoption-soldr.md",
    "docs/consumer-adoption-fbuild.md",
    "docs/v1-broker-architecture.md",
    "docs/v1-admin-verbs.md",
    "docs/v1-backend-lifecycle.md",
    "docs/v1-handoff-optimization.md",
    "docs/v1-observability.md",
    "docs/v1-rollout-policy.md",
    "docs/v1-escape-hatch.md",
    "docs/v1-troubleshooting.md",
];

const REQUIRED_EXAMPLE_FILES: &[&str] = &[
    "examples/README.md",
    "examples/minimal-consumer/Cargo.toml",
    "examples/minimal-consumer/README.md",
    "examples/minimal-consumer/src/main.rs",
    "examples/release-handles-cli/Cargo.toml",
    "examples/release-handles-cli/README.md",
    "examples/release-handles-cli/src/main.rs",
    "examples/custom-isolation/Cargo.toml",
    "examples/custom-isolation/README.md",
    "examples/custom-isolation/src/main.rs",
];

const REQUIRED_CONTRIB_FILES: &[&str] = &[
    "contrib/systemd/running-process-broker-v1.service",
    "contrib/launchd/com.zackees.running-process-broker-v1.plist",
    "contrib/windows-service/install.ps1",
];

const README_EXAMPLE_LINKS: &[&str] = &[
    "examples/minimal-consumer/",
    "examples/release-handles-cli/",
    "examples/custom-isolation/",
];

#[test]
fn required_v1_broker_docs_exist() {
    assert_paths_exist(REQUIRED_V1_DOCS);
}

#[test]
fn consumer_adoption_dashboard_tracks_current_wave() {
    let root = repo_root();
    let dashboard_path = root.join("docs/v1-consumer-adoption-dashboard.md");
    let dashboard = read(&dashboard_path);

    assert_doc_contains_all(
        &dashboard,
        "docs/v1-consumer-adoption-dashboard.md",
        &[
            "Current Minimal-Regime Snapshot",
            "zackees/soldr/pull/721",
            "zackees/soldr/pull/722",
            "zackees/soldr/pull/723",
            "zackees/soldr/pull/724",
            "zackees/zccache/pull/708",
            "zackees/zccache/pull/709",
            "FastLED/fbuild/pull/529",
            "FastLED/fbuild/pull/530",
            "zackees/clud/pull/319",
            "closed minimal",
            "broker_client_wired: false",
            "connect_to_backend",
        ],
    );
}

#[test]
fn required_examples_and_contrib_files_exist() {
    assert_paths_exist(REQUIRED_EXAMPLE_FILES);
    assert_paths_exist(REQUIRED_CONTRIB_FILES);
}

#[test]
fn readmes_link_required_v1_broker_surface() {
    assert_readme_links(
        "README.md",
        "",
        [
            REQUIRED_V1_DOCS,
            README_EXAMPLE_LINKS,
            REQUIRED_CONTRIB_FILES,
        ],
    );
    assert_readme_links(
        "crates/running-process/README.md",
        "../..",
        [
            REQUIRED_V1_DOCS,
            README_EXAMPLE_LINKS,
            REQUIRED_CONTRIB_FILES,
        ],
    );
}

fn assert_paths_exist(paths: &[&str]) {
    let root = repo_root();
    let missing = paths
        .iter()
        .copied()
        .filter(|path| !root.join(path).exists())
        .collect::<Vec<_>>();

    assert!(
        missing.is_empty(),
        "required #240 docs/example/contrib files are missing:\n{}",
        missing.join("\n")
    );
}

fn assert_readme_links<const N: usize>(readme: &str, prefix: &str, groups: [&[&str]; N]) {
    let root = repo_root();
    let readme_path = root.join(readme);
    let text = read(&readme_path);
    let links = markdown_link_targets(&text);
    let mut missing = Vec::new();

    for group in groups {
        for target in group {
            let expected = prefixed_link(prefix, target);
            if !links
                .iter()
                .any(|actual| link_target_matches(actual, &expected))
            {
                missing.push(expected);
            }
        }
    }

    assert!(
        missing.is_empty(),
        "{} must link the required #240 v1 broker docs/example/contrib surface:\n{}",
        display_path(&root, &readme_path),
        missing.join("\n")
    );
}

fn markdown_link_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut cursor = 0;

    while let Some(offset) = text[cursor..].find("](") {
        let target_start = cursor + offset + 2;
        let Some(target_end_offset) = text[target_start..].find(')') else {
            break;
        };
        let raw = &text[target_start..target_start + target_end_offset];
        let target = raw
            .split_whitespace()
            .next()
            .unwrap_or(raw)
            .trim_matches('<')
            .trim_matches('>');
        targets.push(target.to_owned());
        cursor = target_start + target_end_offset + 1;
    }

    targets
}

fn link_target_matches(actual: &str, expected: &str) -> bool {
    actual == expected
        || actual
            .strip_prefix(expected)
            .is_some_and(|suffix| suffix.starts_with('#'))
}

fn prefixed_link(prefix: &str, target: &str) -> String {
    if prefix.is_empty() {
        target.to_owned()
    } else {
        format!("{prefix}/{target}")
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "expected documentation index file to be readable at {}: {err}",
            path.display()
        )
    })
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn assert_doc_contains_all(text: &str, label: &str, required: &[&str]) {
    let missing = required
        .iter()
        .copied()
        .filter(|needle| !text.contains(needle))
        .collect::<Vec<_>>();

    assert!(
        missing.is_empty(),
        "{label} is missing current #242 adoption dashboard markers:\n{}",
        missing.join("\n")
    );
}
