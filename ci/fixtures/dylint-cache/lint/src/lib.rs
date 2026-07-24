#![feature(rustc_private)]

extern crate rustc_span;

use clippy_utils::diagnostics::span_lint;
use rustc_hir::Item;
use rustc_lint::LateLintPass;

dylint_linting::declare_late_lint! {
    pub SOLDR_DYLINT_FIXTURE,
    Warn,
    "cache acceptance fixture"
}

impl<'tcx> LateLintPass<'tcx> for SoldrDylintFixture {
    fn check_item(&mut self, cx: &rustc_lint::LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        if item.ident.name.as_str() == "dylint_fixture_violation" {
            span_lint(
                cx,
                SOLDR_DYLINT_FIXTURE,
                item.span,
                "soldr Dylint fixture diagnostic",
            );
        }
    }
}
