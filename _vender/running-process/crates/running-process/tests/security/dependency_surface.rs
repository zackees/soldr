use std::collections::BTreeSet;
use std::path::Path;

const DEPENDENCY_SURFACE_DOC: &str = include_str!("../../../../docs/v1-dependency-surface.md");

#[test]
fn dependency_surface_doc_matches_runtime_manifest() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest_path.display()));

    let manifest_dependencies = string_set(
        manifest_runtime_dependency_names(&manifest),
        "runtime manifest dependencies",
    );
    let documented_dependencies = string_set(
        documented_runtime_dependency_names(DEPENDENCY_SURFACE_DOC),
        "documented dependency inventory",
    );

    assert!(
        !documented_dependencies.is_empty(),
        "docs/v1-dependency-surface.md must list direct runtime dependencies"
    );
    assert_eq!(
        documented_dependencies, manifest_dependencies,
        "docs/v1-dependency-surface.md drifted from crates/running-process/Cargo.toml"
    );
}

#[test]
fn dependency_surface_doc_records_review_commitments() {
    assert!(
        DEPENDENCY_SURFACE_DOC.contains(
            "No other current direct runtime dependency is reviewed as an HTTP, TLS,"
        ),
        "dependency surface doc must record the no-network direct-dependency review (with #445 ureq carve-out)"
    );
    assert!(
        DEPENDENCY_SURFACE_DOC.contains(
            "The broker wire format remains prost-only; bincode is not present as a direct"
        ),
        "dependency surface doc must record the prost-only wire-format commitment"
    );
    assert!(
        DEPENDENCY_SURFACE_DOC.contains("Update this inventory in the same PR."),
        "dependency surface doc must tell reviewers how to keep the inventory current"
    );
}

fn manifest_runtime_dependency_names(manifest: &str) -> Vec<String> {
    let mut section = Section::Other;
    let mut names = Vec::new();

    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = Section::from_header(trimmed);
            continue;
        }

        if !section.is_runtime_dependencies() || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some((name, _)) = trimmed.split_once('=') {
            names.push(name.trim().trim_matches('"').to_owned());
        }
    }

    names
}

fn documented_runtime_dependency_names(doc: &str) -> Vec<String> {
    let mut in_inventory = false;
    let mut names = Vec::new();

    for line in doc.lines() {
        let trimmed = line.trim();
        if trimmed == "## Direct Runtime Dependency Inventory" {
            in_inventory = true;
            continue;
        }
        if in_inventory && trimmed.starts_with("## ") {
            break;
        }
        if !in_inventory || !trimmed.starts_with('|') {
            continue;
        }

        let cells = trimmed
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect::<Vec<_>>();
        if cells.len() < 4 || cells[0] == "Dependency" || cells[0].starts_with("---") {
            continue;
        }

        names.push(backtick_value(cells[0]).unwrap_or_else(|| {
            panic!("dependency inventory row must start with a backtick-wrapped name: {trimmed}")
        }));
    }

    names
}

fn backtick_value(cell: &str) -> Option<String> {
    let start = cell.find('`')?;
    let rest = &cell[start + 1..];
    let end = rest.find('`')?;
    Some(rest[..end].to_owned())
}

fn string_set(values: Vec<String>, label: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for value in values {
        assert!(
            set.insert(value.clone()),
            "{label} contains duplicate {value}"
        );
    }
    set
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    DirectDependencies,
    TargetDirectDependencies,
    Other,
}

impl Section {
    fn from_header(header: &str) -> Self {
        if header == "[dependencies]" {
            return Self::DirectDependencies;
        }
        if header.starts_with("[target.") && header.ends_with(".dependencies]") {
            return Self::TargetDirectDependencies;
        }
        Self::Other
    }

    fn is_runtime_dependencies(self) -> bool {
        matches!(
            self,
            Self::DirectDependencies | Self::TargetDirectDependencies
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{documented_runtime_dependency_names, manifest_runtime_dependency_names};

    #[test]
    fn manifest_parser_reads_only_runtime_sections() {
        let names = manifest_runtime_dependency_names(
            r#"
            [dependencies]
            prost = "0.14"

            [build-dependencies]
            reqwest = "0.12"

            [dev-dependencies]
            hyper = "1"

            [target.'cfg(windows)'.dependencies]
            windows-sys = "0.59"
            "#,
        );

        assert_eq!(names, ["prost", "windows-sys"]);
    }

    #[test]
    fn doc_parser_reads_inventory_table() {
        let names = documented_runtime_dependency_names(
            r#"
            # v1 Dependency Surface

            ## Direct Runtime Dependency Inventory

            | Dependency | Manifest section | Activation | Review note |
            |---|---|---|---|
            | `prost` | `[dependencies]` | `client` | parser |
            | `windows-sys` | `[target.'cfg(windows)'.dependencies]` | Windows | APIs |

            ## Current Review Summary
            "#,
        );

        assert_eq!(names, ["prost", "windows-sys"]);
    }
}
