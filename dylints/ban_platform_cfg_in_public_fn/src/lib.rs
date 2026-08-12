#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_errors;
extern crate rustc_span;

use rustc_ast::ast::{AssocItem, AssocItemKind, Item, ItemKind};
use rustc_errors::DiagDecorator;
use rustc_lint::{EarlyContext, EarlyLintPass, LintContext};
use rustc_span::{FileName, RemapPathScopeComponents, Span};

#[derive(Default)]
struct BanPlatformCfgInPublicFn {
    modules: Vec<String>,
}

dylint_linting::impl_pre_expansion_lint! {
    /// ### What it does
    ///
    /// Rejects platform selection inside functions whose visibility escapes
    /// their immediate module. Stable outer functions must delegate to
    /// private cfg-selected implementations with a shared signature.
    pub BAN_PLATFORM_CFG_IN_PUBLIC_FN,
    Deny,
    "require platform-neutral public and crate-visible function facades",
    BanPlatformCfgInPublicFn::default()
}

const ALLOWLIST: &str = include_str!("allowlist.txt");

impl EarlyLintPass for BanPlatformCfgInPublicFn {
    fn check_item(&mut self, cx: &EarlyContext<'_>, item: &Item) {
        if let ItemKind::Fn(function) = &item.kind {
            check_function(
                cx,
                item.span,
                &item.attrs,
                function.ident.name.as_str(),
                &self.modules,
            );
        }
        if let ItemKind::Mod(_, ident, _) = &item.kind {
            self.modules.push(ident.name.as_str().to_owned());
        }
    }

    fn check_item_post(&mut self, _cx: &EarlyContext<'_>, item: &Item) {
        if matches!(item.kind, ItemKind::Mod(..)) {
            self.modules.pop();
        }
    }

    fn check_impl_item(&mut self, cx: &EarlyContext<'_>, item: &AssocItem) {
        if let AssocItemKind::Fn(function) = &item.kind {
            check_function(
                cx,
                item.span,
                &item.attrs,
                function.ident.name.as_str(),
                &self.modules,
            );
        }
    }
}

fn check_function(
    cx: &EarlyContext<'_>,
    item_span: Span,
    attributes: &[rustc_ast::ast::Attribute],
    name: &str,
    modules: &[String],
) {
    let filename = source_filename(cx, item_span);
    let Ok(mut source) = cx.sess().source_map().span_to_snippet(item_span) else {
        return;
    };
    if !visibility_escapes_module(&source) {
        return;
    }
    let mut attribute_source = String::new();
    for attribute in attributes {
        if let Ok(snippet) = cx.sess().source_map().span_to_snippet(attribute.span()) {
            source.push_str(&snippet);
            attribute_source.push_str(&snippet);
        }
    }
    if is_test_only(&attribute_source) || modules.iter().any(|module| module == "tests") {
        return;
    }
    let Some(key) = allowlist_key(&filename, name, modules, &attribute_source) else {
        return;
    };
    if !contains_platform_cfg(&source) || is_allowlisted(&key) {
        return;
    }

    cx.opt_span_lint(
        BAN_PLATFORM_CFG_IN_PUBLIC_FN,
        Some(item_span),
        DiagDecorator(move |diag| {
            diag.primary_message(format!(
                "`{key}` selects a platform inside a non-private function; keep the outer body platform-neutral and delegate to cfg-selected private implementations"
            ));
        }),
    );
}

fn visibility_escapes_module(source: &str) -> bool {
    let compact: String = source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    compact.starts_with("pub")
        && !compact.starts_with("pub(self)")
        && !compact.starts_with("pub(inself)")
}

fn is_test_only(source: &str) -> bool {
    let compact: String = source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    compact.contains("#[cfg(test)]")
}

fn is_allowlisted(key: &str) -> bool {
    ALLOWLIST
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .any(|line| line == key)
}

fn allowlist_key(
    filename: &str,
    name: &str,
    modules: &[String],
    attributes: &str,
) -> Option<String> {
    let normalized = filename.replace('\\', "/");
    if normalized.starts_with("ui/")
        || normalized.contains("/ui/")
        || normalized == "$DIR"
        || normalized.starts_with("$DIR/")
    {
        return Some(qualified_key("$DIR", name, modules, attributes));
    }
    let marker = "crates/soldr-cli/src/";
    let offset = normalized.find(marker)?;
    let relative = &normalized[offset..];
    if relative.ends_with("_tests.rs")
        || relative.ends_with("/tests.rs")
        || relative.contains("/tests/")
    {
        return None;
    }
    Some(qualified_key(relative, name, modules, attributes))
}

fn qualified_key(prefix: &str, name: &str, modules: &[String], attributes: &str) -> String {
    let mut key = prefix.to_owned();
    for module in modules {
        key.push_str("::");
        key.push_str(module);
    }
    key.push_str("::");
    key.push_str(name);
    let qualifiers = platform_cfg_qualifiers(attributes);
    if !qualifiers.is_empty() {
        key.push('@');
        key.push_str(&qualifiers.join("+"));
    }
    key
}

fn contains_platform_cfg(source: &str) -> bool {
    !platform_cfg_invocations(source, &["#[cfg(", "#[cfg_attr(", "cfg!("]).is_empty()
}

fn platform_cfg_invocations(source: &str, starts: &[&str]) -> Vec<String> {
    let code = code_without_comments_or_strings(source);
    let compact: String = code
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let selectors = [
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
    let mut invocations = Vec::new();
    for start in starts {
        for (offset, _) in compact.match_indices(start) {
            let Some(clause) = balanced_invocation(&compact, offset) else {
                continue;
            };
            if selectors.iter().any(|selector| clause.contains(selector)) {
                invocations.push(clause.trim_start_matches("#[").to_owned());
            }
        }
    }
    invocations
}

fn platform_cfg_qualifiers(attributes: &str) -> Vec<String> {
    let compact: String = attributes
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let selectors = [
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
    let mut qualifiers = Vec::new();
    for start in ["#[cfg(", "#[cfg_attr("] {
        for (offset, _) in compact.match_indices(start) {
            let Some(clause) = balanced_invocation(&compact, offset) else {
                continue;
            };
            if selectors.iter().any(|selector| clause.contains(selector)) {
                qualifiers.push(clause.trim_start_matches("#[").to_owned());
            }
        }
    }
    qualifiers
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
    assert!(!contains_platform_cfg(
        r####"pub fn neutral() {
            let _ = "#[cfg(windows)]";
            let _ = r###"cfg!(target_arch = "x86_64")"###;
            /* cfg!(unix) */
        }"####
    ));
    assert!(contains_platform_cfg(
        "pub fn selected() { if cfg!(windows) {} }"
    ));
    assert!(contains_platform_cfg(
        "#[cfg_attr(all(feature = \"x\"), cfg(windows))] pub fn nested() {}"
    ));
    assert!(contains_platform_cfg(
        "#[cfg_attr(all(feature = \"x\"), cfg(target_abi = \"eabihf\"))] pub fn abi() {}"
    ));
}
