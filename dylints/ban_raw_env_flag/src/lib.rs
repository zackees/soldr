#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_errors;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_lint::{EarlyContext, EarlyLintPass, LintContext};
use rustc_span::{FileName, RemapPathScopeComponents, Span};
use std::collections::HashSet;

#[derive(Default)]
struct BanRawEnvFlag {
    scanned_files: HashSet<String>,
}

dylint_linting::impl_pre_expansion_lint! {
    /// ### What it does
    ///
    /// Keeps "is this environment variable on" to one definition
    /// (`crates/soldr-core/src/core/env_flag.rs`), by banning hand-rolled
    /// spelling sets everywhere else.
    ///
    /// ### Why
    ///
    /// soldr#2740: five hand-rolled truthy parsers plus inline spellings,
    /// mutually disagreeing. Each looked correct in isolation; the defect
    /// existed only *between* them:
    ///
    /// * `SOLDR_USE_SYSTEM_CMAKE=false` **enabled** the switch, routing
    ///   around the pinned, sha256-verified SDK -- that parser's falsy set
    ///   was `{empty, "0"}` and never excluded the word `false`.
    /// * `ZCCACHE_DISABLE=off` **disabled** the cache -- that parser
    ///   excluded `0` and `false` but not `no`/`off`.
    ///
    /// Detection is on the spelling sets rather than on `env::var`, because
    /// the bug is the *interpretation*, not the read: plenty of legitimate
    /// `env::var` calls return paths and numbers.
    ///
    /// ### Instead
    ///
    /// `soldr_core::core::env_flag`, choosing per variable:
    /// `flag` (owned, default off), `!is_off_value(..)` (owned, default on),
    /// `foreign_flag` (value space owned by another project).
    pub BAN_RAW_ENV_FLAG,
    Deny,
    "keep environment-flag parsing inside soldr-core::core::env_flag",
    BanRawEnvFlag::default()
}

/// The two canonical sets. A hand-rolled parser reproduces one of them --
/// that is what makes this greppable, and what makes a *partial* copy (the
/// soldr#2740 bug) worth catching too.
/// Adjacent pairs from the two canonical sets, whitespace removed.
///
/// Detection is on `|`-alternations rather than on the words appearing
/// anywhere, because a *test* that asserts the contract legitimately lists
/// every spelling -- in an array, comma-separated. A copied parser writes
/// them as match alternatives. Looking for the pipe is what separates
/// "re-implements the rule" from "checks the rule".
///
/// ### Known limit
///
/// A parser that never spells an alternation -- soldr#2740's
/// `!v.is_empty() && v != "0"` -- is not caught by this. That form is a
/// *partial* set rather than a copied one, and catching it textually would
/// mean flagging every comparison against `"0"`. The accessor exists so
/// that form has no reason to be written; this lint catches the copy-paste
/// that actually recurred.
const ON_PAIRS: [&str; 3] = ["\"1\"|\"true\"", "\"true\"|\"yes\"", "\"yes\"|\"on\""];
const OFF_PAIRS: [&str; 4] = [
    "\"\"|\"0\"",
    "\"0\"|\"false\"",
    "\"false\"|\"no\"",
    "\"no\"|\"off\"",
];

impl EarlyLintPass for BanRawEnvFlag {
    fn check_item(&mut self, cx: &EarlyContext<'_>, item: &rustc_ast::ast::Item) {
        let current_file = source_filename(cx, item.span);
        if !in_scope(&current_file) || !self.scanned_files.insert(current_file.clone()) {
            return;
        }
        let source = std::fs::read_to_string(&current_file)
            .or_else(|_| cx.sess().source_map().span_to_snippet(item.span));
        let Ok(source) = source else {
            return;
        };
        for detail in hand_rolled_spelling_sets(&source) {
            emit(cx, item.span, detail);
        }
    }
}

fn emit(cx: &EarlyContext<'_>, span: Span, detail: String) {
    cx.opt_span_lint(
        BAN_RAW_ENV_FLAG,
        Some(span),
        DiagDecorator(move |diag| {
            diag.primary_message(format!(
                "hand-rolled environment-flag parsing: {detail}. Use \
                 `soldr_core::core::env_flag` -- `flag` for an owned switch \
                 that defaults off, `!is_off_value(..)` for one that defaults \
                 on, `foreign_flag` for a variable another project defines \
                 (soldr#2740)"
            ));
        }),
    );
}

/// Every `crates/*/src/**.rs`, except the one file that owns the sets.
///
/// Test modules inside `src/` are in scope on purpose: a test that
/// re-implements the parser validates a copy, which is precisely how the
/// soldr#2740 divergence went unnoticed.
fn in_scope(filename: &str) -> bool {
    let normalized = filename.replace('\\', "/");
    if normalized.ends_with("ui/allowed_env_flag.rs") {
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
    // The definition site.
    if relative.ends_with("crates/soldr-core/src/core/env_flag.rs") {
        return false;
    }
    // soldr#2740: `PYO3_NO_PYTHON`-style detection matches
    // `"1" | "true" | "yes" | "sysroot"` -- a tri-state whose third arm is a
    // domain value, not a flag spelling. Routing it through `flag` would
    // silently drop `sysroot`, so it stays hand-written and exempt. This is
    // the allowlist working as intended: an entry is a reviewed exception,
    // not a way to defer migration.
    !relative.ends_with("crates/soldr-cli/src/pyo3_detect.rs")
}

/// Alternations that look like a copied flag parser.
fn hand_rolled_spelling_sets(source: &str) -> Vec<String> {
    let code = strip_line_comments(source);
    let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let mut found = Vec::new();
    for (label, pairs) in [("on", &ON_PAIRS[..]), ("off", &OFF_PAIRS[..])] {
        let hits: Vec<&str> = pairs
            .iter()
            .copied()
            .filter(|pair| compact.contains(pair))
            .collect();
        if !hits.is_empty() {
            found.push(format!(
                "{} -spelling alternation {} written locally",
                label,
                hits.join(", ")
            ));
        }
    }
    found
}

/// Drop `//` comments so prose naming the spellings does not trip the lint.
/// Doc comments explaining the contract are exactly where these words
/// belong.
fn strip_line_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| match line.find("//") {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copied_on_set_alternation_is_caught() {
        let source = r#"matches!(v, "1" | "true" | "yes" | "on")"#;
        assert_eq!(hand_rolled_spelling_sets(source).len(), 1);
    }

    /// soldr#2740's defects were INCOMPLETE sets, so a partial copy has to
    /// trip the lint too -- requiring the full set would miss them.
    #[test]
    fn an_incomplete_off_set_alternation_is_still_caught() {
        let source = r#"!matches!(v, "0" | "false" | "no")"#;
        assert_eq!(hand_rolled_spelling_sets(source).len(), 1);
    }

    /// The reason detection is on alternations: a test that asserts the
    /// contract lists every spelling, comma-separated, and is legitimate.
    #[test]
    fn a_test_array_listing_the_spellings_is_not_flagged() {
        let source = r#"for v in ["1", "true", "yes", "on"] { assert!(flag_value(v)); }"#;
        assert!(hand_rolled_spelling_sets(source).is_empty());
        let falsy = r#"for v in ["", "0", "false", "no", "off"] { assert!(!flag_value(v)); }"#;
        assert!(hand_rolled_spelling_sets(falsy).is_empty());
    }

    #[test]
    fn an_unrelated_two_value_match_does_not_fire() {
        let source = r#"matches!(mode, "on" | "auto")"#;
        assert!(hand_rolled_spelling_sets(source).is_empty());
    }

    /// Doc comments describing the contract must not trip it.
    #[test]
    fn prose_naming_the_spellings_is_ignored() {
        let source = "// accepts \"1\" | \"true\" | \"yes\" | \"on\"
let x = 1;";
        assert!(hand_rolled_spelling_sets(source).is_empty());
    }

    #[test]
    fn the_definition_site_is_out_of_scope() {
        assert!(!in_scope("crates/soldr-core/src/core/env_flag.rs"));
        assert!(in_scope("crates/soldr-cli/src/wrapper.rs"));
    }
}
