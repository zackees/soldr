//! Source-policy guard for daemon-owned `state.sqlite3` access (soldr#2252).
//!
//! The daemon is the production owner of state.sqlite3. A CLI fallback may touch
//! it only as an explicit offline operation: it must hold the same root lock
//! as the daemon and mark the exact opener as an audited exception. This
//! lightweight scan keeps a future direct-open fallback from silently
//! reintroducing competing redb owners.

use std::fs;
use std::path::{Path, PathBuf};

use crate::common;

const OFFLINE_OWNER_MARKER: &str = "soldr-state-db: offline-root-owner";
const FORBIDDEN_OPENERS: &[&str] = &[
    "TargetRegistry::open(",
    "StateDb::open(",
    "db::open_handle(",
    "db::get_build(",
    "db::aggregate_session(",
    "db::upsert_build(",
    "db::list_builds(",
    "cook_evict_pass(",
    "db::prune_events_older_than(",
    "history_gc::sweep(",
];
const OFFLINE_OWNER_FUNCTIONS: &[&str] = &[
    "offline_registry_rows",
    "daemon_remove_registry_rows",
    "run_offline_cook_gc",
    "run_offline_daemon_event_prune",
    "persist_build_log_history_inner",
];

#[derive(Debug)]
struct FunctionRange {
    name: String,
    start: usize,
    end: usize,
}

fn collect_rs_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn matching_brace(source: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, byte) in source[open..].bytes().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn function_ranges(source: &str) -> Vec<FunctionRange> {
    let mut ranges = Vec::new();
    let mut search_from = 0;
    while let Some(found) = source[search_from..].find("fn ") {
        let start = search_from + found;
        let after_fn = start + 3;
        let Some(name) = source[after_fn..]
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .next()
            .filter(|name| !name.is_empty())
        else {
            search_from = after_fn;
            continue;
        };
        let Some(open_relative) = source[after_fn..].find('{') else {
            break;
        };
        let open = after_fn + open_relative;
        let Some(end) = matching_brace(source, open) else {
            break;
        };
        ranges.push(FunctionRange {
            name: name.to_string(),
            start,
            end: end + 1,
        });
        search_from = end + 1;
    }
    ranges
}

fn is_cfg_test_function(
    source: &str,
    functions: &[FunctionRange],
    function: &FunctionRange,
) -> bool {
    let prior_end = functions
        .iter()
        .filter(|prior| prior.end <= function.start)
        .map(|prior| prior.end)
        .max()
        .unwrap_or(0);
    let item_prefix = &source[prior_end..function.start];
    let Some((before_attribute, after_attribute)) = item_prefix.rsplit_once("#[cfg(test)]") else {
        return false;
    };
    if !before_attribute
        .rsplit_once('\n')
        .map_or(before_attribute, |(_, line)| line)
        .trim()
        .is_empty()
    {
        return false;
    }
    let visibility = after_attribute.trim();
    visibility.is_empty()
        || visibility == "pub"
        || (visibility.starts_with("pub(") && visibility.ends_with(')'))
}

fn cfg_test_module_ranges(source: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut search_from = 0;
    while let Some(attribute) = source[search_from..].find("#[cfg(test)]") {
        let attribute = search_from + attribute;
        let Some(module) = source[attribute..].find("mod ") else {
            break;
        };
        let module = attribute + module;
        let Some(open_relative) = source[module..].find('{') else {
            break;
        };
        let open = module + open_relative;
        let Some(end) = matching_brace(source, open) else {
            break;
        };
        ranges.push((open, end + 1));
        search_from = end + 1;
    }
    ranges
}

fn line_start(source: &str, offset: usize) -> usize {
    source[..offset].rfind('\n').map_or(0, |index| index + 1)
}

fn marker_precedes(source: &str, offset: usize) -> bool {
    let start = line_start(source, offset);
    source[..start]
        .trim_end()
        .rsplit_once('\n')
        .is_some_and(|(_, line)| line.trim() == format!("// {OFFLINE_OWNER_MARKER}"))
}

fn validate_source(path: &str, source: &str) -> Result<(), Vec<String>> {
    let functions = function_ranges(source);
    let test_modules = cfg_test_module_ranges(source);
    let mut offenders = Vec::new();
    let mut line_offset = 0usize;
    for (line_number, raw_line) in source.split_inclusive('\n').enumerate() {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let code = line.split_once("//").map_or(line, |(code, _)| code);
        for opener in FORBIDDEN_OPENERS {
            let Some(column) = code.find(opener) else {
                continue;
            };
            let offset = line_offset + column;
            let function = functions
                .iter()
                .find(|function| function.start <= offset && offset < function.end);
            let in_test_source = path.ends_with("/tests.rs")
                || path.ends_with("_tests.rs")
                || test_modules
                    .iter()
                    .any(|(start, end)| *start <= offset && offset < *end);
            let allowed = in_test_source
                || function.is_some_and(|function| {
                    is_cfg_test_function(source, &functions, function)
                        || (OFFLINE_OWNER_FUNCTIONS.contains(&function.name.as_str())
                            && source[function.start..function.end]
                                .contains("RootOwnershipGuard::try_acquire")
                            && marker_precedes(source, offset))
                });
            if !allowed {
                offenders.push(format!(
                    "{path}:{}: {opener} must use daemon IPC; an offline exception needs \
                     RootOwnershipGuard::try_acquire and // {OFFLINE_OWNER_MARKER}",
                    line_number + 1
                ));
            }
        }
        line_offset += raw_line.len();
    }
    if offenders.is_empty() {
        Ok(())
    } else {
        Err(offenders)
    }
}

#[test]
fn daemon_owns_all_production_state_db_openers() {
    let root = common::crate_root();
    let source_root = root.join("src");
    let mut files = Vec::new();
    collect_rs_files(&source_root, &mut files);

    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&root)
            .expect("source path under crate root")
            .to_string_lossy()
            .replace('\\', "/");
        let source = fs::read_to_string(&file).expect("read CLI source");
        if let Err(mut found) = validate_source(&relative, &source) {
            offenders.append(&mut found);
        }
    }

    assert!(
        offenders.is_empty(),
        "daemon-only state DB ownership violations (soldr#2252):\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn guard_rejects_uncoordinated_production_open_fixture() {
    let source = "fn direct_open() { TargetRegistry::open(&db_path).unwrap(); }";
    let error = validate_source("src/future_fallback.rs", source)
        .expect_err("uncoordinated production direct-open fixture must fail");
    assert!(error[0].contains("must use daemon IPC"));
}

#[test]
fn guard_rejects_indirect_openers_and_does_not_leak_test_attributes() {
    for source in [
        "fn direct_open() { crate::daemon::db::get_build(&db_path, 1).unwrap(); }",
        "fn direct_sweep() { crate::daemon::history_gc::sweep(paths, &db_path, options); }",
        "#[cfg(test)]\nfn prior_test() {}\nfn production() { \
         crate::daemon::db::get_build(&db_path, 1).unwrap(); }",
        "#[cfg(test)]\nconst TEST_ONLY: () = ();\nfn production() { \
         crate::daemon::db::get_build(&db_path, 1).unwrap(); }",
        "// #[cfg(test)]\nfn production() { \
         crate::daemon::db::get_build(&db_path, 1).unwrap(); }",
    ] {
        assert!(
            validate_source("src/future_fallback.rs", source).is_err(),
            "fixture must be rejected: {source}"
        );
    }
}

#[test]
fn guard_permits_only_a_cfg_test_module() {
    let source = "#[cfg(test)]\nmod tests {\nfn seed() { \
                  crate::daemon::db::upsert_build(&db_path, &record).unwrap();\n}\n}";
    assert!(validate_source("src/feature/tests.rs", source).is_ok());
}

#[test]
fn guard_permits_split_test_leaves() {
    let source = "fn seed() { crate::daemon::db::upsert_build(&db_path, &record).unwrap(); }";
    assert!(validate_source("src/logs_cmd_tests.rs", source).is_ok());
    assert!(
        validate_source("src/logs_cmd.rs", source).is_err(),
        "the same opener must remain forbidden in a production leaf"
    );
}

#[test]
fn guard_preserves_offsets_with_crlf_source() {
    let source = "fn offline_registry_rows() {\r\n\
                  let _guard = RootOwnershipGuard::try_acquire();\r\n\
                  // soldr-state-db: offline-root-owner\r\n\
                  TargetRegistry::open(&db_path);\r\n\
                  }\r\n";
    assert!(validate_source("src/gc/mod.rs", source).is_ok());
}
