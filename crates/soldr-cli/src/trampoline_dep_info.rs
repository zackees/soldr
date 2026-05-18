//! Cargo `.d` dep-info parser used by the `soldr cargo run` trampoline
//! (issue #344). Pulled out of [`crate::trampoline`] to keep that file
//! under the 1000-LOC ceiling (post-#339 convention).
//!
//! cargo emits one `<output>: <space-separated source paths>` line per
//! output artifact. Lines may end with `\` to continue onto the next
//! line; embedded spaces in paths are escaped as `\ `. The exact grammar
//! is documented in cargo's `src/cargo/util/dep_info.rs`. We aim for
//! tolerance over strict conformance — on any unexpected input the
//! parser returns `None` and callers fall through silently to real
//! cargo.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Parse a cargo `.d` dep-info file and return the source file list for the
/// stanza whose left-hand side (output) matches `binary`. On any parse
/// failure we return `None`.
pub(crate) fn parse_dep_info_for_output(text: &str, binary: &Path) -> Option<Vec<PathBuf>> {
    let logical = join_continuations(text);
    let stanzas = parse_stanzas(&logical);
    let raw_sources = select_stanza(&stanzas, binary)?;

    let mut deduped: BTreeSet<PathBuf> = BTreeSet::new();
    for raw in raw_sources {
        let p = PathBuf::from(raw);
        if !p.as_os_str().is_empty() {
            deduped.insert(p);
        }
    }
    Some(deduped.into_iter().collect())
}

fn join_continuations(text: &str) -> Vec<String> {
    let mut pending = String::new();
    let mut out = Vec::new();
    for raw in text.lines() {
        if let Some(stripped) = raw.strip_suffix('\\') {
            pending.push_str(stripped);
            pending.push(' ');
            continue;
        }
        pending.push_str(raw);
        out.push(std::mem::take(&mut pending));
    }
    if !pending.is_empty() {
        out.push(pending);
    }
    out
}

fn parse_stanzas(logical: &[String]) -> Vec<(String, Vec<String>)> {
    let mut stanzas: Vec<(String, Vec<String>)> = Vec::new();
    for line in logical {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((lhs, rhs)) = split_dep_info_line(line) else {
            continue;
        };
        let sources = tokenize_dep_info_paths(&rhs);
        stanzas.push((lhs, sources));
    }
    stanzas
}

fn select_stanza(stanzas: &[(String, Vec<String>)], binary: &Path) -> Option<Vec<String>> {
    let canonical_bin = fs::canonicalize(binary).ok();
    let binary_filename = binary.file_name().and_then(|s| s.to_str());

    for (lhs, sources) in stanzas {
        let lhs_path = PathBuf::from(lhs);
        let match_full = canonical_bin
            .as_ref()
            .and_then(|c| fs::canonicalize(&lhs_path).ok().map(|l| l == *c))
            .unwrap_or(false);
        let match_str = lhs_path == *binary;
        let match_name = binary_filename
            .map(|n| lhs_path.file_name().and_then(|s| s.to_str()) == Some(n))
            .unwrap_or(false);
        if match_full || match_str || match_name {
            return Some(sources.clone());
        }
    }
    // Fallback: a single stanza is unambiguous.
    if stanzas.len() == 1 {
        return Some(stanzas[0].1.clone());
    }
    None
}

/// Split a dep-info line on the first unescaped colon that separates the
/// output from the source list. Windows drive letters (`C:`) need careful
/// handling — a single-letter prefix followed by a colon is treated as
/// part of the path.
fn split_dep_info_line(line: &str) -> Option<(String, String)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if c == b':' && !is_drive_letter_colon(bytes, i) {
            return Some((
                line[..i].trim().to_string(),
                line[i + 1..].trim().to_string(),
            ));
        }
        i += 1;
    }
    None
}

fn is_drive_letter_colon(bytes: &[u8], i: usize) -> bool {
    if i + 1 >= bytes.len() {
        return false;
    }
    let separator = bytes[i + 1];
    if separator != b'\\' && separator != b'/' {
        return false;
    }
    let at_start = i == 1 && bytes[0].is_ascii_alphabetic();
    let after_space =
        i >= 2 && bytes[i - 2].is_ascii_whitespace() && bytes[i - 1].is_ascii_alphabetic();
    at_start || after_space
}

/// Tokenize the space-separated dep-info RHS, honoring `\ ` (escaped
/// space) as a literal space inside a path.
fn tokenize_dep_info_paths(rhs: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let bytes = rhs.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if matches!(next, b' ' | b'\t' | b'\\' | b'#' | b':') {
                let ch = if next == b':' { ':' } else { next as char };
                current.push(ch);
                i += 2;
                continue;
            }
            current.push('\\');
            i += 1;
            continue;
        }
        if c == b' ' || c == b'\t' {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            i += 1;
            continue;
        }
        current.push(c as char);
        i += 1;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}
