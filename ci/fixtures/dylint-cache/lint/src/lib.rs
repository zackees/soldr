#![feature(rustc_private)]

extern crate rustc_hir;

use rustc_hir::Item;
use rustc_lint::LateLintPass;

dylint_linting::declare_late_lint! {
    pub SOLDR_DYLINT_FIXTURE,
    Warn,
    "cache acceptance fixture"
}

impl<'tcx> LateLintPass<'tcx> for SoldrDylintFixture {
    fn check_item(&mut self, cx: &rustc_lint::LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        if cx
            .tcx
            .def_path_str(item.owner_id.to_def_id())
            .ends_with("dylint_fixture_violation")
        {
            cx.tcx
                .dcx()
                .span_warn(item.span, "soldr Dylint fixture diagnostic");
        }
    }
}
