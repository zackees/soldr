#![feature(rustc_private)]

extern crate rustc_span;

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
            cx.tcx
                .dcx()
                .span_warn(item.span, "soldr Dylint fixture diagnostic");
        }
    }
}
