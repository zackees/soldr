//! Unit coverage split from `build_log.rs` for the soldr#2493
//! 1,000-line production-source ceiling.

use super::*;

fn sample_request<'a>(
    paths: &'a SoldrPaths,
    cwd: &'a Path,
    args: &'a [String],
) -> BuildLogRequest<'a> {
    BuildLogRequest {
        paths,
        session_id: 42,
        cwd,
        args,
        started_at_ms: 1_700_000_000_000,
        ended_at_ms: 1_700_000_005_000,
        exit_code: 0,
        compile_journal_path: None,
        compile_journal_start_len: 0,
        // soldr#1799: absent by default so the existing cases keep
        // asserting the shape of a log without toolchain telemetry --
        // `None` must stay renderable, since it is what a build whose
        // soldr root failed to resolve produces.
        toolchain: None,
        wrapper: None,
        fingerprint_dirty: Vec::new(),
    }
}

#[test]
fn toolchain_homes_render_when_present_and_vanish_when_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let args = vec!["cargo".to_string(), "build".to_string()];

    // Absent: no element at all, rather than an element claiming an
    // origin nobody established. soldr#1799's CI check treats a missing
    // <toolchain> as "not asserted"; a fabricated one would read as a
    // pass.
    let request = sample_request(&paths, tmp.path(), &args);
    let without = write_build_log(&request).expect("write");
    let raw = std::fs::read_to_string(&without).expect("read");
    assert!(
        !raw.contains("<toolchain"),
        "absent telemetry must emit no element, got:
{raw}"
    );

    // Present: origin and the binary that justifies it.
    let mut request = sample_request(&paths, tmp.path(), &args);
    request.toolchain = Some(ToolchainHomes {
        home_origin: "caller",
        binary: PathBuf::from("/usr/bin/cargo"),
    });
    let with = write_build_log(&request).expect("write");
    let raw = std::fs::read_to_string(&with).expect("read");
    assert!(
        raw.contains("home_origin=\"caller\""),
        "expected the caller origin, got:
{raw}"
    );
    assert!(
        raw.contains("cargo"),
        "expected the resolved binary, got:
{raw}"
    );
}

#[test]
fn wrapper_identity_renders_when_present_and_vanishes_when_absent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let args = vec!["cargo".to_string(), "build".to_string()];

    // Absent stays absent (soldr#2545): same not-asserted contract as
    // <toolchain>.
    let request = sample_request(&paths, tmp.path(), &args);
    let without = write_build_log(&request).expect("write");
    let raw = std::fs::read_to_string(&without).expect("read");
    assert!(
        !raw.contains("<wrapper"),
        "got:
{raw}"
    );

    // Managed identity carries origin + effective path.
    let mut request = sample_request(&paths, tmp.path(), &args);
    request.wrapper = Some(WrapperIdentity {
        effective: Some(PathBuf::from("/root/.soldr/v1/shims/rustc")),
        origin: "soldr-managed",
    });
    let with = write_build_log(&request).expect("write");
    let raw = std::fs::read_to_string(&with).expect("read");
    assert!(
        raw.contains("origin=\"soldr-managed\""),
        "got:
{raw}"
    );
    assert!(
        raw.contains("shims"),
        "got:
{raw}"
    );

    // Disabled records the origin with no effective attribute.
    let mut request = sample_request(&paths, tmp.path(), &args);
    request.wrapper = Some(WrapperIdentity {
        effective: None,
        origin: "disabled",
    });
    let disabled = write_build_log(&request).expect("write");
    let raw = std::fs::read_to_string(&disabled).expect("read");
    assert!(
        raw.contains("origin=\"disabled\""),
        "got:
{raw}"
    );
    assert!(
        !raw.contains("effective="),
        "got:
{raw}"
    );
}

#[test]
fn write_build_log_writes_file_with_expected_header() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().join("soldr-root"));
    let cwd_dir = tmp.path().join("project");
    std::fs::create_dir_all(&cwd_dir).expect("mkdir cwd");
    let args = vec!["cargo".to_string(), "build".to_string()];
    let request = sample_request(&paths, &cwd_dir, &args);

    let path = write_build_log(&request).expect("write_build_log");
    assert!(path.is_file(), "log file must exist: {}", path.display());
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("xml"));

    let raw = std::fs::read_to_string(&path).expect("read log");
    assert!(
        raw.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"),
        "must start with the XML declaration: {raw}"
    );
    assert!(raw.contains("schema_version=\"1\""), "{raw}");
    assert!(
        raw.contains(&format!(
            "cwd=\"{}\"",
            xml_escape_attr(&cwd_dir.display().to_string())
        )),
        "{raw}"
    );
    assert!(raw.contains("<arg>cargo</arg>"), "{raw}");
    assert!(raw.contains("<arg>build</arg>"), "{raw}");
    assert!(raw.contains("wall_ms=\"5000\""), "totals wall_ms: {raw}");
    // Empty compile/download groups render self-closing (no
    // <item> children).
    assert!(!raw.contains("<item"), "no items expected: {raw}");
    assert!(raw.contains("derived=\"true\""), "{raw}");
    // The compile AND link group nodes both carry the derived
    // build-settings attributes (owner's load-bearing
    // requirement — settings stamped on both groups).
    for group in ["<compile", "<link"] {
        let start = raw
            .find(group)
            .unwrap_or_else(|| panic!("{group} missing: {raw}"));
        let end = raw[start..]
            .find(['>', '/'])
            .map(|i| start + i)
            .unwrap_or(raw.len());
        let head = &raw[start..end];
        for attr_name in ["target=", "profile=", "debug=", "opt_level=", "lto="] {
            assert!(
                head.contains(attr_name),
                "{group} node missing {attr_name}: {head}"
            );
        }
    }
}

#[test]
fn write_build_log_without_daemon_never_opens_state_db() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().join("soldr-root"));
    let cwd_dir = tmp.path().join("project");
    std::fs::create_dir_all(&cwd_dir).expect("mkdir cwd");
    let args = vec![
        "soldr".to_string(),
        "cargo".to_string(),
        "build".to_string(),
    ];
    let request = sample_request(&paths, &cwd_dir, &args);

    let path = write_build_log(&request).expect("best-effort log");
    let xml = std::fs::read_to_string(path).expect("read log");
    assert!(
        xml.contains("history_source=\"daemon-unavailable\""),
        "daemon-less log must visibly identify incomplete history: {xml}"
    );
    assert!(
        !crate::cache_lib::data_db_path(&paths).exists(),
        "a daemon-less build log must stay incomplete instead of opening state.sqlite3"
    );
}

#[test]
fn filename_shape_starts_with_compact_timestamp_and_slug() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path().join("builds");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let cwd = PathBuf::from("C:\\Users\\niteris\\dev\\soldr2");
    let started_at_ms = 1_700_000_000_000_i64;

    let first = unique_filename(&dir, started_at_ms, &cwd);
    let ts = utc_compact_timestamp(started_at_ms);
    assert_eq!(ts.len(), 16, "compact timestamp must be 16 chars: {ts}");
    assert!(
        first.starts_with(&ts),
        "filename must start with the compact timestamp: {first}"
    );
    assert!(first.ends_with(".xml"));
    let slug = sanitize_cwd_slug(&cwd);
    assert!(
        first.contains(&slug),
        "filename must contain the sanitized cwd slug: {first}"
    );

    // Simulate a collision: create the exact filename and confirm
    // the next call appends "-2".
    std::fs::write(dir.join(&first), b"<build/>").expect("write collision file");
    let second = unique_filename(&dir, started_at_ms, &cwd);
    assert_ne!(first, second);
    assert!(
        second.ends_with("-2.xml"),
        "collision must append -2: {second}"
    );
}

#[test]
fn derive_build_meta_reads_release_debug_and_target_flags() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let cwd = tmp.path();

    let release_args = vec![
        "cargo".to_string(),
        "build".to_string(),
        "--release".to_string(),
    ];
    let meta = derive_build_meta(&release_args, cwd);
    assert_eq!(meta.profile, "release");
    assert_eq!(meta.opt_level, "3");
    assert!(!meta.debug);

    let bench_args = vec![
        "cargo".to_string(),
        "build".to_string(),
        "--profile".to_string(),
        "bench".to_string(),
    ];
    let meta = derive_build_meta(&bench_args, cwd);
    assert_eq!(meta.profile, "bench");

    let target_args = vec![
        "cargo".to_string(),
        "build".to_string(),
        "--target".to_string(),
        "x86_64-unknown-linux-gnu".to_string(),
    ];
    let meta = derive_build_meta(&target_args, cwd);
    assert_eq!(meta.target, "x86_64-unknown-linux-gnu");

    let default_args = vec!["cargo".to_string(), "build".to_string()];
    let meta = derive_build_meta(&default_args, cwd);
    assert_eq!(meta.profile, "debug");
    assert!(meta.debug);
    assert_eq!(meta.opt_level, "0");
}

#[test]
fn derive_build_meta_reads_lto_from_cargo_toml() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let cwd = tmp.path();
    std::fs::write(
        cwd.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[profile.release]\nlto = \"thin\"\n",
    )
    .expect("write Cargo.toml");

    let args = vec![
        "cargo".to_string(),
        "build".to_string(),
        "--release".to_string(),
    ];
    let meta = derive_build_meta(&args, cwd);
    assert_eq!(meta.lto, "thin");
}

#[test]
fn prune_build_logs_keeps_newest_n() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path().join("builds");
    std::fs::create_dir_all(&dir).expect("mkdir");

    let total = BUILD_LOG_KEEP + 5;
    let mut names = Vec::new();
    for i in 0..total {
        let name = format!("{:020}-project.json", i);
        std::fs::write(dir.join(&name), b"{}").expect("write fixture");
        names.push(name);
    }

    let deleted = prune_build_logs(&dir, BUILD_LOG_KEEP);
    assert_eq!(deleted, 5);

    let remaining: std::collections::HashSet<String> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(remaining.len(), BUILD_LOG_KEEP);

    // The newest BUILD_LOG_KEEP (highest-numbered) names must survive.
    for name in names.iter().skip(5) {
        assert!(
            remaining.contains(name),
            "newest file should survive prune: {name}"
        );
    }
    for name in names.iter().take(5) {
        assert!(
            !remaining.contains(name),
            "oldest file should be pruned: {name}"
        );
    }
}

#[test]
fn prune_build_logs_matches_both_xml_and_legacy_json() {
    // Legacy `.json` files (written by interim builds before the
    // JSON->XML conversion) must still be swept alongside current
    // `.xml` files, and unrelated extensions must be left alone.
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path().join("builds");
    std::fs::create_dir_all(&dir).expect("mkdir");

    std::fs::write(dir.join("20260101T000000Z-a.xml"), b"<build/>").expect("write xml");
    std::fs::write(dir.join("20260101T000001Z-b.json"), b"{}").expect("write json");
    std::fs::write(dir.join("readme.txt"), b"not a log").expect("write txt");

    let deleted = prune_build_logs(&dir, 0);
    assert_eq!(
        deleted, 2,
        "both the xml and legacy json log must be pruned"
    );

    let remaining: Vec<String> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        remaining,
        vec!["readme.txt".to_string()],
        "non-log extensions must be left alone: {remaining:?}"
    );
}

#[test]
fn xml_escape_attr_escapes_reserved_and_control_chars() {
    assert_eq!(xml_escape_attr("plain"), "plain");
    assert_eq!(xml_escape_attr("a & b"), "a &amp; b");
    assert_eq!(xml_escape_attr("<tag>"), "&lt;tag&gt;");
    assert_eq!(xml_escape_attr("say \"hi\""), "say &quot;hi&quot;");
    assert_eq!(xml_escape_attr("it's"), "it&apos;s");
    // Control char (0x01) other than tab/newline is escaped;
    // tab and newline pass through unescaped.
    assert_eq!(xml_escape_attr("a\u{1}b"), "a&#x01;b");
    assert_eq!(xml_escape_attr("a\tb\nc"), "a\tb\nc");
}

#[test]
fn write_build_log_escapes_ampersand_and_quote_in_cwd_and_args() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().join("soldr-root"));
    // Directory names can't literally contain `"` on Windows, so
    // exercise the escaper against the raw cwd string embedded in
    // the request rather than an actual created directory — the
    // writer only needs `request.cwd` for rendering the `cwd`
    // attribute and the filename slug, both of which tolerate a
    // synthetic (non-existent) path fine for this test.
    let raw_cwd = tmp.path().join("a & b");
    std::fs::create_dir_all(&raw_cwd).expect("mkdir cwd");
    let args = vec![
        "cargo".to_string(),
        "build".to_string(),
        "--message-format=\"json\"".to_string(),
    ];
    let request = sample_request(&paths, &raw_cwd, &args);

    let path = write_build_log(&request).expect("write_build_log");
    let raw = std::fs::read_to_string(&path).expect("read log");

    // The escaped forms are present...
    assert!(raw.contains("a &amp; b"), "{raw}");
    assert!(raw.contains("--message-format=&quot;json&quot;"), "{raw}");
    // ...and the raw, unescaped forms are not (would produce
    // malformed XML).
    assert!(!raw.contains("a & b\""), "{raw}");
    assert!(!raw.contains("--message-format=\"json\""), "{raw}");
}

#[test]
fn compile_journal_cache_outcomes_map_hit_and_miss() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let journal_path = tmp.path().join("compile_journal.jsonl");
    let lines = [
        r#"{"ts":"2026-01-01T00:00:00Z","outcome":"hit","compiler":"/rustc","args":[],"cwd":"/repo","exit_code":0,"session_id":null,"latency_ns":1000,"crate_name":"hit-crate"}"#,
        r#"{"ts":"2026-01-01T00:00:01Z","outcome":"miss","compiler":"/rustc","args":[],"cwd":"/repo","exit_code":0,"session_id":null,"latency_ns":2000,"crate_name":"miss-crate","miss_reason":"context_not_found"}"#,
        r#"not json at all"#,
        r#"{"ts":"2026-01-01T00:00:02Z","outcome":"error","compiler":"/rustc","args":[],"cwd":"/repo","exit_code":1,"session_id":null,"latency_ns":500,"crate_name":"error-crate"}"#,
    ];
    let body = lines.join("\n") + "\n";
    std::fs::write(&journal_path, &body).expect("write journal");

    let outcomes = read_compile_cache_outcomes(Some(&journal_path), 0);
    assert_eq!(outcomes.get("hit-crate").map(String::as_str), Some("hit"));
    assert_eq!(outcomes.get("miss-crate").map(String::as_str), Some("miss"));
    assert_eq!(
        outcomes.get("error-crate").map(String::as_str),
        Some("unknown")
    );
    assert!(!outcomes.contains_key("never-seen-crate"));

    // Byte-offset support: entries before `compile_journal_start_len`
    // must be ignored (they belong to a previous session sharing
    // the journal file).
    let offset = body.find("miss-crate").map(|i| i as u64).unwrap_or(0);
    // Back up to the start of that line.
    let line_start = body[..offset as usize]
        .rfind('\n')
        .map(|i| i as u64 + 1)
        .unwrap_or(0);
    let tail_only = read_compile_cache_outcomes(Some(&journal_path), line_start);
    assert!(!tail_only.contains_key("hit-crate"));
    assert_eq!(
        tail_only.get("miss-crate").map(String::as_str),
        Some("miss")
    );
}

#[test]
fn build_compile_items_pairs_start_and_end_events() {
    let events = vec![
        Event {
            ts_ms: 1_000,
            session_id: Some(7),
            kind: EventKind::CompileStart,
            crate_name: Some("crate-a".into()),
            duration_us: None,
            target_dir: None,
            exit_code: None,
        },
        Event {
            ts_ms: 1_500,
            session_id: Some(7),
            kind: EventKind::CompileEnd,
            crate_name: Some("crate-a".into()),
            duration_us: Some(500_000),
            target_dir: None,
            exit_code: None,
        },
        Event {
            ts_ms: 1_600,
            session_id: Some(7),
            kind: EventKind::CompileEnd,
            crate_name: Some("crate-b".into()),
            duration_us: Some(100_000),
            target_dir: None,
            exit_code: None,
        },
    ];
    let mut outcomes = HashMap::new();
    outcomes.insert("crate-a".to_string(), "hit".to_string());
    let (items, wall_ms, cpu_ms) = build_compile_items(&events, &outcomes);
    assert_eq!(items.len(), 2);
    assert_eq!(wall_ms, 600); // 1_600 - 1_000
    assert_eq!(cpu_ms, 600); // 500 + 100
    let crate_a = items
        .iter()
        .find(|item| item.crate_name == "crate-a")
        .expect("crate-a item");
    assert_eq!(crate_a.cache, "hit");
    let crate_b = items
        .iter()
        .find(|item| item.crate_name == "crate-b")
        .expect("crate-b item");
    assert_eq!(crate_b.cache, "unknown");

    let link = build_link_step(&events);
    assert_eq!(link.items.len(), 1);
    assert_eq!(link.items[0].crate_name, "crate-b");
    assert!(link.derived);
}

#[test]
fn fingerprint_dirty_section_renders_when_present_and_vanishes_when_absent() {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let cwd = root.path().join("proj");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let args = vec!["cargo".to_string(), "build".to_string()];
    let mut request = sample_request(&paths, &cwd, &args);

    let without = write_build_log(&request).expect("write");
    let raw = std::fs::read_to_string(&without).expect("read");
    assert!(!raw.contains("<fingerprint_dirty"), "{raw}");

    request.fingerprint_dirty = vec![FingerprintDirty {
        name: "serde".into(),
        version: "1.0.0".into(),
        reason: "the file `src/lib.rs` has changed (1 < 2)".into(),
    }];
    let with = write_build_log(&request).expect("write");
    let raw = std::fs::read_to_string(&with).expect("read");
    assert!(raw.contains("  <fingerprint_dirty>\n"), "{raw}");
    assert!(
        raw.contains(
            "    <unit name=\"serde\" version=\"1.0.0\" reason=\"the file `src/lib.rs` has changed (1 &lt; 2)\" />\n"
        ),
        "{raw}"
    );
}
