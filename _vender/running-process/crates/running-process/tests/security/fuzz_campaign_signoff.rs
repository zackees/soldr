use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const FUZZ_SIGNOFF_DOC: &str = include_str!("../../../../docs/v1-fuzz-campaign-signoff.md");
const MIN_RELEASE_FUZZ_SECONDS: u64 = 3600;

#[test]
fn fuzz_campaign_signoff_targets_match_fuzz_manifest() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest_path = crate_root.join("fuzz").join("Cargo.toml");
    let manifest = read_to_string(&manifest_path);

    let manifest_targets = string_set(&fuzz_manifest_target_names(&manifest), "manifest targets");
    assert!(
        !manifest_targets.is_empty(),
        "{} must declare cargo-fuzz bin targets",
        manifest_path.display()
    );

    let campaign_rows = fuzz_campaign_rows(FUZZ_SIGNOFF_DOC);
    let campaign_targets = campaign_rows.keys().cloned().collect::<BTreeSet<_>>();

    assert_eq!(
        campaign_targets,
        manifest_targets,
        "docs/v1-fuzz-campaign-signoff.md target matrix drifted from fuzz/Cargo.toml\n\
         missing from doc: {:?}\n\
         extra in doc: {:?}",
        manifest_targets
            .difference(&campaign_targets)
            .collect::<Vec<_>>(),
        campaign_targets
            .difference(&manifest_targets)
            .collect::<Vec<_>>()
    );

    for (target, row) in &campaign_rows {
        assert!(
            row.minimum_seconds >= MIN_RELEASE_FUZZ_SECONDS,
            "{target} release evidence must require at least {MIN_RELEASE_FUZZ_SECONDS} seconds"
        );
        assert!(
            matches!(row.status.as_str(), "pending" | "passed"),
            "{target} status must be pending or passed, got {:?}",
            row.status
        );
        if row.status == "passed" {
            assert_release_url(target, &row.release_run_url);
            assert!(
                row.corpus_or_artifact != "TBD",
                "{target} passed status must include a corpus or artifact reference"
            );
        }
    }
}

#[test]
fn fuzz_campaign_signoff_records_release_evidence_fields() {
    assert!(
        FUZZ_SIGNOFF_DOC.contains("Release gate status: PENDING"),
        "signoff artifact must not claim #241 completion before final evidence lands"
    );
    assert!(
        FUZZ_SIGNOFF_DOC.contains("security-fuzz` workflow run with `fuzz_seconds=3600`"),
        "signoff artifact must require a one-hour workflow-dispatch campaign"
    );
    assert!(
        FUZZ_SIGNOFF_DOC.contains("one matrix job per fuzz target"),
        "signoff artifact must require matrixed per-target release evidence"
    );
    assert!(
        FUZZ_SIGNOFF_DOC.contains("release-fuzz-evidence-<target>"),
        "signoff artifact must name the per-target release evidence artifact convention"
    );

    for field in REQUIRED_RELEASE_EVIDENCE_FIELDS {
        assert!(
            table_has_first_column_value(FUZZ_SIGNOFF_DOC, field),
            "required release evidence field missing: {field}"
        );
    }

    for field in REQUIRED_REVIEWER_SIGNOFF_FIELDS {
        assert!(
            table_has_first_column_value(FUZZ_SIGNOFF_DOC, field),
            "required reviewer signoff field missing: {field}"
        );
    }
}

#[test]
fn security_fuzz_workflow_supports_one_hour_release_dispatch() {
    let workflow_path = repo_root(Path::new(env!("CARGO_MANIFEST_DIR")))
        .join(".github")
        .join("workflows")
        .join("security-fuzz.yml");
    let workflow = read_to_string(&workflow_path);

    for needle in [
        "workflow_dispatch:",
        "fuzz_seconds:",
        "Use 3600 for v1 release signoff evidence.",
        r#"default: "3600""#,
        r#"fuzz_seconds="${{ inputs.fuzz_seconds }}""#,
        r#"fuzz_seconds="${fuzz_seconds:-3600}""#,
        r#"if [[ "${{ github.event_name }}" == "workflow_dispatch" && "${fuzz_seconds}" -ge 3600 ]]; then"#,
        "name: release-fuzz-evidence-${{ matrix.target }}",
    ] {
        assert!(
            workflow.contains(needle),
            "security-fuzz workflow must support one-hour release dispatch: missing {needle}"
        );
    }
}

const REQUIRED_RELEASE_EVIDENCE_FIELDS: &[&str] = &[
    "release_candidate_commit",
    "security_fuzz_workflow_run",
    "cargo_audit_run",
    "security_test_run",
    "cve_regression_run",
    "dependency_surface_review",
    "unsafe_inventory_review",
    "privileged_operation_review",
    "input_boundary_review",
];

const REQUIRED_REVIEWER_SIGNOFF_FIELDS: &[&str] = &[
    "reviewer_name",
    "reviewer_affiliation",
    "review_date",
    "reviewed_commit",
    "final_decision",
    "reviewer_notes",
];

#[derive(Debug)]
struct FuzzCampaignRow {
    minimum_seconds: u64,
    release_run_url: String,
    corpus_or_artifact: String,
    status: String,
}

fn fuzz_campaign_rows(doc: &str) -> BTreeMap<String, FuzzCampaignRow> {
    let mut rows = BTreeMap::new();

    for line in doc.lines() {
        let cells = markdown_table_cells(line);
        if cells.len() != 5 {
            continue;
        }

        let Some(target) = backtick_value(&cells[0]) else {
            continue;
        };
        if !target.starts_with("fuzz_") {
            continue;
        }

        let minimum_seconds = cells[1].parse::<u64>().unwrap_or_else(|err| {
            panic!(
                "fuzz campaign row for {target} has invalid minimum_seconds {:?}: {err}",
                cells[1]
            )
        });
        let previous = rows.insert(
            target.clone(),
            FuzzCampaignRow {
                minimum_seconds,
                release_run_url: cells[2].clone(),
                corpus_or_artifact: cells[3].clone(),
                status: cells[4].to_ascii_lowercase(),
            },
        );
        assert!(
            previous.is_none(),
            "duplicate fuzz campaign row for {target}"
        );
    }

    assert!(
        !rows.is_empty(),
        "docs/v1-fuzz-campaign-signoff.md must include a fuzz campaign matrix"
    );
    rows
}

fn fuzz_manifest_target_names(manifest: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut in_bin = false;

    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed == "[[bin]]" {
            in_bin = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_bin = false;
            continue;
        }
        if !in_bin {
            continue;
        }
        if let Some(name) = toml_string_assignment(trimmed, "name") {
            targets.push(name);
        }
    }

    targets
}

fn toml_string_assignment(line: &str, key: &str) -> Option<String> {
    let (lhs, rhs) = line.split_once('=')?;
    if lhs.trim() != key {
        return None;
    }

    let value = rhs.trim().strip_prefix('"')?;
    let end = value
        .find('"')
        .unwrap_or_else(|| panic!("unterminated TOML string assignment for {key}"));
    Some(value[..end].to_owned())
}

fn table_has_first_column_value(doc: &str, expected: &str) -> bool {
    doc.lines().any(|line| {
        let cells = markdown_table_cells(line);
        cells.first().is_some_and(|cell| cell == expected)
    })
}

fn markdown_table_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') {
        return Vec::new();
    }
    let cells = trimmed
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect::<Vec<_>>();
    if cells
        .iter()
        .all(|cell| cell.chars().all(|ch| ch == '-' || ch == ':'))
    {
        return Vec::new();
    }
    cells
}

fn backtick_value(cell: &str) -> Option<String> {
    let value = cell.strip_prefix('`')?.strip_suffix('`')?;
    Some(value.to_owned())
}

fn assert_release_url(target: &str, value: &str) {
    assert!(
        value.starts_with("https://github.com/zackees/running-process/actions/runs/"),
        "{target} passed status must link a running-process Actions run, got {value:?}"
    );
}

fn string_set(values: &[String], label: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for value in values {
        assert!(
            set.insert(value.clone()),
            "{label} must not contain duplicate fuzz target entry {value}"
        );
    }
    set
}

fn repo_root(crate_root: &Path) -> PathBuf {
    crate_root
        .parent()
        .and_then(|path| path.parent())
        .unwrap_or_else(|| panic!("failed to derive repo root from {}", crate_root.display()))
        .to_path_buf()
}

fn read_to_string(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{fuzz_campaign_rows, fuzz_manifest_target_names};

    #[test]
    fn campaign_row_parser_reads_fuzz_target_rows() {
        let rows = fuzz_campaign_rows(
            r#"
            | Target | minimum_seconds | release_run_url | corpus_or_artifact | status |
            |---|---:|---|---|---|
            | `fuzz_one` | 3600 | TBD | TBD | pending |
            "#,
        );

        let row = rows.get("fuzz_one").expect("missing fuzz_one row");
        assert_eq!(row.minimum_seconds, 3600);
        assert_eq!(row.release_run_url, "TBD");
        assert_eq!(row.corpus_or_artifact, "TBD");
        assert_eq!(row.status, "pending");
    }

    #[test]
    fn manifest_parser_reads_fuzz_bin_names() {
        let targets = fuzz_manifest_target_names(
            r#"
            [package]
            name = "running-process-fuzz"

            [[bin]]
            name = "fuzz_one"
            path = "fuzz_targets/fuzz_one.rs"

            [[bin]]
            name = "fuzz_two"
            path = "fuzz_targets/fuzz_two.rs"
            "#,
        );

        assert_eq!(targets, ["fuzz_one", "fuzz_two"]);
    }
}
