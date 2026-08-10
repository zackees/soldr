use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
struct FuzzTarget {
    name: String,
    path: String,
}

#[test]
fn security_fuzz_workflow_targets_match_fuzz_manifest() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fuzz_root = crate_root.join("fuzz");
    let manifest_path = fuzz_root.join("Cargo.toml");
    let workflow_path = repo_root(crate_root)
        .join(".github")
        .join("workflows")
        .join("security-fuzz.yml");

    let manifest = read_to_string(&manifest_path);
    let workflow = read_to_string(&workflow_path);

    let manifest_targets = fuzz_manifest_targets(&manifest);
    assert!(
        !manifest_targets.is_empty(),
        "{} must declare cargo-fuzz bin targets",
        manifest_path.display()
    );

    let manifest_target_names = target_name_set(&manifest_targets);
    let workflow_target_names = string_set(&workflow_fuzz_targets(&workflow), "workflow targets");
    assert_eq!(
        workflow_target_names,
        manifest_target_names,
        "security-fuzz workflow target list drifted from fuzz/Cargo.toml\n\
         missing from workflow: {:?}\n\
         extra in workflow: {:?}",
        manifest_target_names
            .difference(&workflow_target_names)
            .collect::<Vec<_>>(),
        workflow_target_names
            .difference(&manifest_target_names)
            .collect::<Vec<_>>()
    );

    let manifest_target_paths = manifest_targets
        .iter()
        .map(|target| normalize_path(Path::new(&target.path)))
        .collect::<Vec<_>>();
    let _ = string_set(&manifest_target_paths, "manifest target paths");

    let missing_target_files = manifest_target_paths
        .iter()
        .filter(|path| !fuzz_root.join(path).is_file())
        .collect::<Vec<_>>();
    assert!(
        missing_target_files.is_empty(),
        "fuzz/Cargo.toml bin target paths must exist: {missing_target_files:#?}"
    );

    let mismatched_stems = manifest_targets
        .iter()
        .filter_map(|target| {
            let stem = Path::new(&target.path)
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or("");
            (stem != target.name).then(|| format!("{} -> {}", target.name, target.path))
        })
        .collect::<Vec<_>>();
    assert!(
        mismatched_stems.is_empty(),
        "cargo-fuzz bin target names must match their file stems: {mismatched_stems:#?}"
    );
}

#[test]
fn security_fuzz_workflow_has_required_ci_controls() {
    let workflow_path = repo_root(Path::new(env!("CARGO_MANIFEST_DIR")))
        .join(".github")
        .join("workflows")
        .join("security-fuzz.yml");
    let workflow = read_to_string(&workflow_path);

    assert_yaml_key(&workflow, "pull_request");
    assert_yaml_key(&workflow, "schedule");
    assert_yaml_key(&workflow, "workflow_dispatch");
    assert_contains(
        &workflow,
        "strategy:",
        "workflow must run fuzz targets as independent matrix jobs",
    );
    assert_contains(
        &workflow,
        "fail-fast: false",
        "workflow matrix must keep collecting evidence after one target fails",
    );
    assert_contains(
        &workflow,
        "name: cargo-fuzz (${{ matrix.target }})",
        "workflow job name must identify the matrix fuzz target",
    );
    assert_contains(
        &workflow,
        "path: crates/running-process/fuzz/corpus/${{ matrix.target }}",
        "workflow must cache the fuzz corpus path per target",
    );
    assert_contains(
        &workflow,
        "uses: actions/upload-artifact@v4",
        "workflow must upload artifacts on fuzz failures",
    );
    assert_contains(
        &workflow,
        "path: crates/running-process/fuzz/artifacts/${{ matrix.target }}",
        "workflow must upload cargo-fuzz artifacts per target",
    );
    assert_contains(
        &workflow,
        r#"if [[ "${{ github.event_name }}" == "pull_request" ]]; then"#,
        "workflow must distinguish pull_request runs from longer runs",
    );
    assert_contains(
        &workflow,
        r#"fuzz_seconds="${FUZZ_SECONDS:-30}""#,
        "pull_request fuzz runs must default to 30 seconds per target",
    );
    assert_contains(
        &workflow,
        r#"fuzz_seconds="${fuzz_seconds:-3600}""#,
        "workflow_dispatch fuzz runs must default to one hour per target",
    );
    assert_contains(
        &workflow,
        r#"fuzz_seconds="${FUZZ_SECONDS:-1800}""#,
        "scheduled fuzz runs must default to 1800 seconds per target",
    );
    assert_contains(
        &workflow,
        r#"release_evidence=true"#,
        "release dispatch runs must mark evidence uploads when fuzz_seconds is at least 3600",
    );
    assert_contains(
        &workflow,
        "name: release-fuzz-evidence-${{ matrix.target }}",
        "successful release fuzz runs must upload per-target evidence artifacts",
    );

    let fuzz_run = workflow
        .lines()
        .find(|line| line.contains("cargo +nightly fuzz run"))
        .unwrap_or_else(|| panic!("workflow must run cargo +nightly fuzz run"));
    assert!(
        fuzz_run.contains(r#"-max_total_time="${FUZZ_SECONDS}""#),
        "cargo-fuzz run must use -max_total_time from FUZZ_SECONDS: {fuzz_run}"
    );
    assert!(
        fuzz_run.contains(r#""${{ matrix.target }}""#),
        "cargo-fuzz run must execute exactly one matrix target per job: {fuzz_run}"
    );
    assert!(
        fuzz_run.contains("-timeout=30"),
        "cargo-fuzz run must keep per-input timeout at 30 seconds: {fuzz_run}"
    );
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

fn fuzz_manifest_targets(manifest: &str) -> Vec<FuzzTarget> {
    let mut targets = Vec::new();
    let mut current = None;

    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed == "[[bin]]" {
            push_manifest_target(&mut targets, current.take());
            current = Some(PartialFuzzTarget::default());
            continue;
        }
        if trimmed.starts_with('[') {
            push_manifest_target(&mut targets, current.take());
            continue;
        }

        let Some(target) = &mut current else {
            continue;
        };
        if let Some(value) = toml_string_assignment(trimmed, "name") {
            target.name = Some(value);
        } else if let Some(value) = toml_string_assignment(trimmed, "path") {
            target.path = Some(value);
        }
    }

    push_manifest_target(&mut targets, current);
    targets
}

#[derive(Default)]
struct PartialFuzzTarget {
    name: Option<String>,
    path: Option<String>,
}

fn push_manifest_target(targets: &mut Vec<FuzzTarget>, target: Option<PartialFuzzTarget>) {
    let Some(target) = target else {
        return;
    };
    let name = target
        .name
        .unwrap_or_else(|| panic!("fuzz/Cargo.toml [[bin]] entry is missing name"));
    let path = target
        .path
        .unwrap_or_else(|| panic!("fuzz/Cargo.toml [[bin]] entry {name} is missing path"));
    targets.push(FuzzTarget { name, path });
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

fn workflow_fuzz_targets(workflow: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut in_target_list = false;
    let mut target_indent = 0;

    for line in workflow.lines() {
        let trimmed = line.trim();
        if !in_target_list {
            if trimmed != "target:" {
                continue;
            }
            in_target_list = true;
            target_indent = indent_len(line);
            continue;
        }

        if trimmed.is_empty() {
            continue;
        }

        let indent = indent_len(line);
        if indent <= target_indent && !trimmed.starts_with("- ") {
            break;
        }

        let Some(target) = trimmed.strip_prefix("- ") else {
            continue;
        };
        let target = target.trim_matches(|ch| ch == '\'' || ch == '"');
        if !target.is_empty() {
            targets.push(target.to_owned());
        }
    }

    if targets.is_empty() {
        panic!("security-fuzz workflow must declare a matrix.target list");
    }

    targets
}

fn indent_len(line: &str) -> usize {
    line.chars().take_while(|ch| *ch == ' ').count()
}

fn target_name_set(targets: &[FuzzTarget]) -> BTreeSet<String> {
    let names = targets
        .iter()
        .map(|target| target.name.clone())
        .collect::<Vec<_>>();
    string_set(&names, "manifest target names")
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

fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn assert_yaml_key(workflow: &str, key: &str) {
    assert!(
        workflow
            .lines()
            .any(|line| line.trim() == format!("{key}:")),
        "security-fuzz workflow must declare {key}"
    );
}

fn assert_contains(haystack: &str, needle: &str, message: &str) {
    assert!(haystack.contains(needle), "{message}: missing {needle}");
}

#[cfg(test)]
mod tests {
    use super::{fuzz_manifest_targets, workflow_fuzz_targets, FuzzTarget};

    #[test]
    fn security_fuzz_workflow_parser_reads_manifest_bin_targets() {
        let targets = fuzz_manifest_targets(
            r#"
            [package]
            name = "running-process-fuzz"

            [[bin]]
            name = "fuzz_one"
            path = "fuzz_targets/fuzz_one.rs"
            test = false

            [[bin]]
            name = "fuzz_two"
            path = "fuzz_targets/fuzz_two.rs"
            "#,
        );

        assert_eq!(
            targets,
            [
                FuzzTarget {
                    name: "fuzz_one".to_string(),
                    path: "fuzz_targets/fuzz_one.rs".to_string()
                },
                FuzzTarget {
                    name: "fuzz_two".to_string(),
                    path: "fuzz_targets/fuzz_two.rs".to_string()
                }
            ]
        );
    }

    #[test]
    fn security_fuzz_workflow_parser_reads_matrix_target_list() {
        let targets = workflow_fuzz_targets(
            r#"
            strategy:
              matrix:
                target:
                  - fuzz_one
                  - "fuzz_two"
            "#,
        );

        assert_eq!(targets, ["fuzz_one", "fuzz_two"]);
    }
}
