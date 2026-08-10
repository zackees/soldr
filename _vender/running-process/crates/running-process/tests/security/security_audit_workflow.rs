const SECURITY_AUDIT_WORKFLOW: &str =
    include_str!("../../../../.github/workflows/security-audit.yml");

const REQUIRED_DEPENDENCY_REVIEW_PATHS: &[&str] = &[
    ".github/workflows/security-audit.yml",
    "Cargo.lock",
    "**/Cargo.toml",
    "docs/v1-security-model.md",
];

#[test]
fn security_audit_workflow_covers_dependency_review_surface() {
    assert_event_has_paths("pull_request", REQUIRED_DEPENDENCY_REVIEW_PATHS);
    assert_event_has_paths("push", REQUIRED_DEPENDENCY_REVIEW_PATHS);

    let push = event_block("push");
    assert!(
        push.contains("branches: [main]"),
        "security audit push trigger must cover main"
    );

    assert!(
        SECURITY_AUDIT_WORKFLOW.contains("workflow_dispatch:"),
        "security audit workflow must support manual dispatch"
    );

    assert_daily_schedule();
}

#[test]
fn security_audit_workflow_denies_audit_warnings() {
    assert!(
        SECURITY_AUDIT_WORKFLOW.contains("run: cargo audit --deny warnings"),
        "security audit workflow must fail on cargo audit warnings"
    );
}

fn assert_event_has_paths(event: &str, required_paths: &[&str]) {
    let block = event_block(event);
    assert!(
        block.contains("paths:"),
        "security audit {event} trigger must be path-scoped"
    );

    for path in required_paths {
        let needle = format!("- \"{path}\"");
        assert!(
            block.contains(&needle),
            "security audit {event} trigger must include path {path:?}"
        );
    }
}

fn assert_daily_schedule() {
    let schedule = event_block("schedule");
    let cron = schedule
        .lines()
        .find_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("- cron: ")
                .map(|value| value.trim_matches('"'))
        })
        .expect("security audit workflow must have a scheduled cron trigger");
    let fields = cron.split_whitespace().collect::<Vec<_>>();

    assert_eq!(
        fields.len(),
        5,
        "security audit cron must use the standard five-field format"
    );
    assert_eq!(
        &fields[2..],
        &["*", "*", "*"],
        "security audit schedule must run daily"
    );
}

fn event_block(event: &str) -> String {
    let header = format!("  {event}:");
    let mut found = false;
    let mut block = Vec::new();

    for line in SECURITY_AUDIT_WORKFLOW.lines() {
        if !found {
            found = line == header;
            continue;
        }
        if (line.starts_with("  ") && !line.starts_with("    "))
            || (!line.starts_with(' ') && !line.trim().is_empty())
        {
            break;
        }
        block.push(line);
    }

    assert!(found, "security audit workflow missing {event} trigger");
    let block = block.join("\n");
    assert!(
        !block.trim().is_empty(),
        "security audit workflow missing {event} trigger"
    );
    block
}
