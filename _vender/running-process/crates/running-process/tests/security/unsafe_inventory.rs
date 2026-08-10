use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

const BROKER_UNSAFE_INVENTORY: &[UnsafeInventoryEntry] = &[
    UnsafeInventoryEntry {
        path: "src/broker/backend_lifecycle/verify_pid.rs",
        unsafe_count: 19,
    },
    UnsafeInventoryEntry {
        // Slice 32 of #500: broker-owned bind. Two `fcntl` sites clear
        // FD_CLOEXEC on the listener being handed over, one `getsockopt`
        // site checks a descriptor is a listening socket, and one
        // `from_raw_fd` adopts it. The adoption is gated on that check —
        // see docs/v1-security-model.md.
        path: "src/broker/broker_owned_bind.rs",
        unsafe_count: 4,
    },
    UnsafeInventoryEntry {
        // The CLOEXEC test reads the flag back with `fcntl(F_GETFD)` on the
        // listener it just created; no ownership is taken.
        path: "src/broker/broker_owned_bind/tests.rs",
        unsafe_count: 1,
    },
    UnsafeInventoryEntry {
        // Slice 4 of #488 (PR #491): `unsafe { libc::getuid() }` in
        // `resolve_socket_path` for the unix bind dir. Same pattern v1
        // uses in `lifecycle/names.rs::unix_broker_socket_dir`.
        path: "src/broker/client_v2.rs",
        unsafe_count: 2,
    },
    UnsafeInventoryEntry {
        path: "src/broker/fs_health.rs",
        unsafe_count: 2,
    },
    UnsafeInventoryEntry {
        path: "src/broker/host_identity.rs",
        unsafe_count: 13,
    },
    UnsafeInventoryEntry {
        path: "src/broker/lifecycle/names.rs",
        unsafe_count: 2,
    },
    UnsafeInventoryEntry {
        path: "src/broker/lifecycle/privilege.rs",
        unsafe_count: 3,
    },
    UnsafeInventoryEntry {
        path: "src/broker/lifecycle/process_tree.rs",
        unsafe_count: 7,
    },
    UnsafeInventoryEntry {
        path: "src/broker/lifecycle/sid.rs",
        unsafe_count: 5,
    },
    UnsafeInventoryEntry {
        path: "src/broker/manifest.rs",
        unsafe_count: 1,
    },
    UnsafeInventoryEntry {
        path: "src/broker/secure_dir.rs",
        unsafe_count: 9,
    },
    UnsafeInventoryEntry {
        path: "src/broker/server/connection.rs",
        unsafe_count: 4,
    },
    UnsafeInventoryEntry {
        path: "src/broker/server/handoff/unix.rs",
        unsafe_count: 11,
    },
    UnsafeInventoryEntry {
        path: "src/broker/server/handoff/windows.rs",
        unsafe_count: 6,
    },
    UnsafeInventoryEntry {
        path: "src/broker/server/spawn_coordinator.rs",
        unsafe_count: 8,
    },
];

#[derive(Clone, Copy, Debug)]
struct UnsafeInventoryEntry {
    path: &'static str,
    unsafe_count: usize,
}

#[test]
fn broker_unsafe_inventory_matches_security_audit() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let broker_root = crate_root.join("src").join("broker");

    let expected = expected_inventory();
    let actual = scan_broker_unsafe_counts(crate_root, &broker_root);

    let unexpected = actual
        .keys()
        .filter(|path| !expected.contains_key(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let missing = expected
        .keys()
        .filter(|path| !actual.contains_key(**path))
        .cloned()
        .collect::<Vec<_>>();
    let drift = expected
        .iter()
        .filter_map(|(path, expected_count)| {
            let actual_count = actual.get(*path)?;
            (actual_count != expected_count).then(|| {
                format!(
                    "{path}: expected {expected_count} unsafe keyword tokens, found {actual_count}"
                )
            })
        })
        .collect::<Vec<_>>();

    assert!(
        unexpected.is_empty() && missing.is_empty() && drift.is_empty(),
        "broker unsafe inventory drifted; update docs/v1-security-model.md and \
         BROKER_UNSAFE_INVENTORY after security review.\n\
         unexpected files: {unexpected:#?}\n\
         missing inventoried files: {missing:#?}\n\
         count drift: {drift:#?}\n\
         actual inventory:\n{}",
        format_inventory(&actual)
    );
}

fn expected_inventory() -> BTreeMap<&'static str, usize> {
    let mut expected = BTreeMap::new();

    for entry in BROKER_UNSAFE_INVENTORY {
        let duplicate = expected.insert(entry.path, entry.unsafe_count);
        assert!(
            duplicate.is_none(),
            "duplicate unsafe inventory entry for {}",
            entry.path
        );
    }

    expected
}

fn scan_broker_unsafe_counts(crate_root: &Path, broker_root: &Path) -> BTreeMap<String, usize> {
    let mut actual = BTreeMap::new();

    for path in rust_files_under(broker_root) {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        let unsafe_count = count_unsafe_keyword_tokens(&source);
        if unsafe_count == 0 {
            continue;
        }

        let relative_path = path
            .strip_prefix(crate_root)
            .unwrap_or_else(|err| panic!("failed to make {} relative: {err}", path.display()));
        actual.insert(normalize_path(relative_path), unsafe_count);
    }

    actual
}

fn rust_files_under(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(path) = pending.pop() {
        let entries = std::fs::read_dir(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));

        for entry in entries {
            let path = entry
                .unwrap_or_else(|err| {
                    panic!("failed to read dir entry in {}: {err}", path.display())
                })
                .path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension() == Some(OsStr::new("rs")) {
                files.push(path);
            }
        }
    }

    files.sort();
    files
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn format_inventory(actual: &BTreeMap<String, usize>) -> String {
    actual
        .iter()
        .map(|(path, count)| format!("    {path}: {count}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn count_unsafe_keyword_tokens(source: &str) -> usize {
    let mut count = 0;
    let mut cursor = 0;

    while cursor < source.len() {
        let rest = &source[cursor..];

        if rest.starts_with("//") {
            cursor = skip_line_comment(source, cursor + 2);
            continue;
        }
        if rest.starts_with("/*") {
            cursor = skip_block_comment(source, cursor + 2);
            continue;
        }
        if let Some(next) = raw_string_end(source, cursor) {
            cursor = next;
            continue;
        }
        if rest.starts_with("b\"") {
            cursor = skip_quoted(source, cursor + 2);
            continue;
        }
        if rest.starts_with('"') {
            cursor = skip_quoted(source, cursor + 1);
            continue;
        }

        let ch = rest
            .chars()
            .next()
            .expect("cursor is always inside a character boundary");
        if is_identifier_start(ch) {
            let token_start = cursor;
            cursor += ch.len_utf8();
            while cursor < source.len() {
                let ch = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is always inside a character boundary");
                if !is_identifier_continue(ch) {
                    break;
                }
                cursor += ch.len_utf8();
            }
            if &source[token_start..cursor] == "unsafe" {
                count += 1;
            }
            continue;
        }

        cursor += ch.len_utf8();
    }

    count
}

fn skip_line_comment(source: &str, mut cursor: usize) -> usize {
    while cursor < source.len() {
        let ch = source[cursor..]
            .chars()
            .next()
            .expect("cursor is always inside a character boundary");
        cursor += ch.len_utf8();
        if ch == '\n' {
            break;
        }
    }
    cursor
}

fn skip_block_comment(source: &str, mut cursor: usize) -> usize {
    let mut depth = 1;

    while cursor < source.len() {
        let rest = &source[cursor..];
        if rest.starts_with("/*") {
            depth += 1;
            cursor += 2;
        } else if rest.starts_with("*/") {
            depth -= 1;
            cursor += 2;
            if depth == 0 {
                break;
            }
        } else {
            let ch = rest
                .chars()
                .next()
                .expect("cursor is always inside a character boundary");
            cursor += ch.len_utf8();
        }
    }

    cursor
}

fn raw_string_end(source: &str, cursor: usize) -> Option<usize> {
    let bytes = source.as_bytes();

    let prefix_len = if bytes.get(cursor) == Some(&b'b') && bytes.get(cursor + 1) == Some(&b'r') {
        2
    } else if bytes.get(cursor) == Some(&b'r') {
        1
    } else {
        return None;
    };

    let mut quote = cursor + prefix_len;
    while bytes.get(quote) == Some(&b'#') {
        quote += 1;
    }
    if bytes.get(quote) != Some(&b'"') {
        return None;
    }

    let hashes = quote - cursor - prefix_len;
    let mut body = quote + 1;
    while body < source.len() {
        if bytes.get(body) == Some(&b'"') {
            let mut matched_hashes = 0;
            while matched_hashes < hashes && bytes.get(body + 1 + matched_hashes) == Some(&b'#') {
                matched_hashes += 1;
            }
            if matched_hashes == hashes {
                return Some(body + 1 + hashes);
            }
        }
        body += 1;
    }

    Some(source.len())
}

fn skip_quoted(source: &str, mut cursor: usize) -> usize {
    while cursor < source.len() {
        let ch = source[cursor..]
            .chars()
            .next()
            .expect("cursor is always inside a character boundary");
        cursor += ch.len_utf8();
        if ch == '\\' {
            if cursor < source.len() {
                let escaped = source[cursor..]
                    .chars()
                    .next()
                    .expect("cursor is always inside a character boundary");
                cursor += escaped.len_utf8();
            }
            continue;
        }
        if ch == '"' {
            break;
        }
    }
    cursor
}

fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_identifier_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::count_unsafe_keyword_tokens;

    #[test]
    fn counts_only_real_unsafe_tokens() {
        let source = r###"
            unsafe fn real_one() {}
            fn real_two() { unsafe { call(); } }
            fn not_real() {
                let unsafe_identifier = "unsafe";
                let raw = r#"unsafe"#;
                // unsafe in a line comment
                /*
                 * unsafe in a block comment
                 */
            }
        "###;

        assert_eq!(count_unsafe_keyword_tokens(source), 2);
    }
}
