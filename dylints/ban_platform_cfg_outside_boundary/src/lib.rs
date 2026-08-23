#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_errors;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_lint::{EarlyContext, EarlyLintPass, LintContext};
use rustc_span::{FileName, RemapPathScopeComponents, Span};
use std::collections::HashSet;

#[derive(Default)]
struct BanPlatformCfgOutsideBoundary {
    scanned_files: HashSet<String>,
}

dylint_linting::impl_pre_expansion_lint! {
    /// ### What it does
    ///
    /// Enforces the #2493 host-selection boundary: the only hand-written
    /// sources allowed to choose the host platform are the `cfg_select!`
    /// block in `crates/soldr-platform/src/lib.rs` and the three concrete
    /// implementation trees (`platform_win`, `platform_linux`,
    /// `platform_macos`). Every other production source file denies
    /// host-platform `#[cfg]`, `#[cfg_attr]`, and `cfg!()`, and every
    /// production source outside `crates/soldr-platform` denies direct
    /// references to the concrete trees (`platform_imp`, `platform_win`,
    /// `platform_linux`, `platform_macos`).
    ///
    /// Inspection happens pre-expansion, so cfg'd-away code cannot hide.
    /// Integration tests, examples, benches, and inline test modules are
    /// scanned as part of the same boundary.
    ///
    /// ### Second-order effect: single-platform CI is sufficient
    ///
    /// This lint is why the other boundary lints can be trusted despite CI
    /// linting exactly one target. Four of the six (`ban_raw_process_creation`,
    /// `ban_raw_network_access`, `ban_raw_ipc_transport`,
    /// `ban_raw_local_socket_name`) are **late-pass**: they see only what
    /// actually compiles for the target being checked, and `ci.yml` runs every
    /// dylint step under `nightly-…-x86_64-unknown-linux-gnu`. Taken alone,
    /// that would leave `#[cfg(windows)]` code permanently unlinted by exactly
    /// the lints guarding raw process, socket, and IPC construction.
    ///
    /// It does not, because this lint runs *pre-expansion* and forbids host
    /// `#[cfg]` outside `soldr-platform`. Host-specific code can therefore only
    /// exist inside the platform crate — which is precisely where raw platform
    /// APIs are legitimate and where the raw-API lints already expect to find
    /// them. There is no third place for an unlinted Windows-only `Command::new`
    /// to live.
    ///
    /// Verified empirically (soldr#2758/#2761 made the toolchain installable on
    /// Windows): building all six lints for `x86_64-pc-windows-msvc` and running
    /// `cargo dylint --all -- --workspace --all-targets` on a Windows host
    /// reports **zero** findings, and `crates/` outside `soldr-platform`
    /// contains zero real `#[cfg(windows)]` attributes.
    ///
    /// The practical consequence: adding a second dylint lane for another host
    /// would buy no coverage. If this lint is ever relaxed, that stops being
    /// true and the raw-API lints silently lose a platform.
    pub BAN_PLATFORM_CFG_OUTSIDE_BOUNDARY,
    Deny,
    "keep host-platform selection inside soldr-platform's cfg_select boundary",
    BanPlatformCfgOutsideBoundary::default()
}

const SELECTORS: [&str; 10] = [
    "windows",
    "unix",
    "target_os",
    "target_family",
    "target_arch",
    "target_abi",
    "target_env",
    "target_vendor",
    "target_endian",
    "target_pointer_width",
];

impl EarlyLintPass for BanPlatformCfgOutsideBoundary {
    fn check_item(&mut self, cx: &EarlyContext<'_>, item: &rustc_ast::ast::Item) {
        let current_file = source_filename(cx, item.span);
        if !in_scope(&current_file) || !self.scanned_files.insert(current_file.clone()) {
            return;
        }
        // Scan the physical source once, rather than only the active AST.
        // This is what makes cfg-elided items and crate-level inner attrs
        // visible to the boundary.
        let source = std::fs::read_to_string(&current_file)
            .or_else(|_| cx.sess().source_map().span_to_snippet(item.span));
        if let Ok(source) = source {
            for invocation in platform_cfg_invocations(&source) {
                emit(cx, item.span, format!("host cfg `{invocation}`"));
            }
            for reference in native_platform_references(&source) {
                emit(cx, item.span, format!("direct native-platform reference `{reference}`"));
            }
            if outside_platform_crate(&current_file) {
                for reference in concrete_tree_references(&source) {
                    emit(cx, item.span, format!("direct concrete-tree reference `{reference}`"));
                }
            }
        }
    }
}

fn emit(cx: &EarlyContext<'_>, span: Span, detail: String) {
    cx.opt_span_lint(
        BAN_PLATFORM_CFG_OUTSIDE_BOUNDARY,
        Some(span),
        DiagDecorator(move |diag| {
            diag.primary_message(format!(
                "host-platform selection outside the soldr-platform boundary: {detail}; \
                 the only allowed selection sites are soldr-platform/src/lib.rs and the \
                 platform_win/platform_linux/platform_macos trees"
            ));
        }),
    );
}

/// Production sources the boundary applies to: every `crates/*/src/**`
/// file (including unit-test modules inside them — test-only exceptions
/// belong in non-production test targets), excluding the generated and
/// vendored trees. The concrete platform trees are the boundary's inside.
fn in_scope(filename: &str) -> bool {
    let normalized = filename.replace('\\', "/");
    if normalized.ends_with("ui/allowed_boundary.rs") {
        return false;
    }
    if normalized.starts_with("ui/") || normalized.contains("/ui/") {
        return true;
    }
    let Some(marker) = normalized.find("crates/") else {
        return false;
    };
    let relative = &normalized[marker..];
    if !relative.ends_with(".rs") {
        return false;
    }
    if relative.starts_with("crates/soldr-platform/src/platform_win")
        || relative.starts_with("crates/soldr-platform/src/platform_linux")
        || relative.starts_with("crates/soldr-platform/src/platform_macos")
        || relative == "crates/soldr-platform/src/lib.rs"
    {
        return false; // the boundary itself
    }
    true
}

/// Direct references to the concrete trees are allowed only inside
/// `crates/soldr-platform` (lib.rs selection site, the facades, and the
/// trees themselves). Downstream crates reach only `crate::platform::…`.
fn outside_platform_crate(filename: &str) -> bool {
    let normalized = filename.replace('\\', "/");
    !normalized.contains("crates/soldr-platform/")
}

fn platform_cfg_invocations(source: &str) -> Vec<String> {
    let code = code_without_comments_or_strings(source);
    let compact: String = code
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let mut invocations = Vec::new();
    for start in ["#[cfg(", "#[cfg_attr(", "#![cfg(", "#![cfg_attr(", "cfg!("] {
        for (offset, _) in compact.match_indices(start) {
            let Some(clause) = balanced_invocation(&compact, offset) else {
                continue;
            };
            if SELECTORS.iter().any(|selector| clause.contains(selector)) {
                invocations.push(clause.trim_start_matches("#[").to_owned());
            }
        }
    }
    invocations
}

fn concrete_tree_references(source: &str) -> Vec<String> {
    let code = code_without_comments_or_strings(source);
    let mut references = Vec::new();
    for name in ["platform_imp", "platform_win", "platform_linux", "platform_macos"] {
        // Word-boundary scan: the identifier must stand alone.
        for (offset, _) in code.match_indices(name) {
            let before = code[..offset].chars().next_back();
            let after = code[offset + name.len()..].chars().next();
            let standalone = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            if standalone(before) && standalone(after) {
                references.push(name.to_owned());
            }
        }
    }
    references
}

fn native_platform_references(source: &str) -> Vec<String> {
    let code = code_without_comments_or_strings(source);
    [
        "std::os::windows",
        "std::os::unix",
        "std::os::linux",
        "std::os::macos",
        "windows_sys",
        "windows::Win32",
        "libc::",
        "tokio::net::windows",
        "tokio::net::Unix",
        "interprocess::os::windows",
        "interprocess::os::unix",
    ]
    .into_iter()
    .filter(|marker| code.contains(marker))
    .map(str::to_owned)
    .collect()
}

fn balanced_invocation(source: &str, offset: usize) -> Option<&str> {
    let mut depth = 0_u32;
    let mut saw_open = false;
    for (relative, character) in source[offset..].char_indices() {
        match character {
            '(' => {
                saw_open = true;
                depth += 1;
            }
            ')' if saw_open => {
                depth -= 1;
                if depth == 0 {
                    return Some(&source[offset..offset + relative + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

fn code_without_comments_or_strings(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut output = String::with_capacity(source.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some((prefix_len, hashes)) = raw_string_prefix(&bytes[index..]) {
            let start = index;
            index += prefix_len;
            while index < bytes.len() {
                let suffix = &bytes[index + 1..];
                if bytes[index] == b'"'
                    && suffix.len() >= hashes
                    && suffix[..hashes].iter().all(|byte| *byte == b'#')
                {
                    index += 1 + hashes;
                    break;
                }
                index += 1;
            }
            mask_range(&mut output, bytes, start, index);
        } else if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                output.push(' ');
                index += 1;
            }
        } else if bytes[index..].starts_with(b"/*") {
            let mut depth = 1_u32;
            output.push_str("  ");
            index += 2;
            while index < bytes.len() && depth > 0 {
                if bytes[index..].starts_with(b"/*") {
                    depth += 1;
                    output.push_str("  ");
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    depth -= 1;
                    output.push_str("  ");
                    index += 2;
                } else {
                    output.push(if bytes[index] == b'\n' { '\n' } else { ' ' });
                    index += 1;
                }
            }
        } else if bytes[index] == b'"' {
            output.push(' ');
            index += 1;
            while index < bytes.len() {
                let byte = bytes[index];
                output.push(if byte == b'\n' { '\n' } else { ' ' });
                index += 1;
                if byte == b'\\' && index < bytes.len() {
                    output.push(' ');
                    index += 1;
                } else if byte == b'"' {
                    break;
                }
            }
        } else {
            output.push(bytes[index] as char);
            index += 1;
        }
    }
    output
}

fn raw_string_prefix(source: &[u8]) -> Option<(usize, usize)> {
    let mut index = usize::from(source.starts_with(b"br"));
    if source.get(index) != Some(&b'r') {
        return None;
    }
    index += 1;
    let hashes_start = index;
    while source.get(index) == Some(&b'#') {
        index += 1;
    }
    (source.get(index) == Some(&b'"')).then_some((index + 1, index - hashes_start))
}

fn mask_range(output: &mut String, source: &[u8], start: usize, end: usize) {
    for byte in &source[start..end] {
        output.push(if *byte == b'\n' { '\n' } else { ' ' });
    }
}

fn source_filename(cx: &EarlyContext<'_>, span: Span) -> String {
    match cx.sess().source_map().span_to_filename(span) {
        FileName::Real(real_filename) => real_filename
            .local_path()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                real_filename
                    .path(RemapPathScopeComponents::DIAGNOSTICS)
                    .to_string_lossy()
                    .into_owned()
            }),
        filename => filename
            .display(RemapPathScopeComponents::DIAGNOSTICS)
            .to_string(),
    }
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}

#[test]
fn source_detector_ignores_comments_and_strings() {
    assert!(platform_cfg_invocations(
        r####"fn neutral() {
            let _ = "#[cfg(windows)]";
            let _ = r###"cfg!(target_arch = "x86_64")"###;
            /* cfg!(unix) */
        }"####
    )
    .is_empty());
    assert_eq!(
        platform_cfg_invocations("fn selected() { if cfg!(windows) {} }"),
        vec!["cfg!(windows)"]
    );
    // feature cfgs are allowed: only the host selectors count
    assert!(platform_cfg_invocations("#[cfg(feature = \"tokio-console\")] fn f() {}").is_empty());
    // test-only cfg is still host selection when a host selector appears
    assert_eq!(
        platform_cfg_invocations("#[cfg(all(test, target_os = \"windows\"))] fn f() {}"),
        vec!["cfg(all(test,target_os=))"]
    );
}

#[test]
fn concrete_references_are_word_boundary_matched() {
    assert_eq!(
        concrete_tree_references("crate::platform_imp::fs::x(); platform_win::y();"),
        vec!["platform_imp", "platform_win"]
    );
    assert!(concrete_tree_references("platform_win32; platform_windows;").is_empty());
}

#[test]
fn native_platform_references_are_detected_outside_strings() {
    assert_eq!(
        native_platform_references("use std::os::unix::fs::PermissionsExt; libc::getpid();"),
        vec!["std::os::unix", "libc::"]
    );
    assert!(native_platform_references("let text = \"windows_sys\";").is_empty());
}

#[test]
fn integration_tests_examples_and_benches_are_in_scope() {
    for path in [
        "crates/demo/tests/host.rs",
        "crates/demo/examples/host.rs",
        "crates/demo/benches/host.rs",
    ] {
        assert!(in_scope(path), "{path}");
    }
}
