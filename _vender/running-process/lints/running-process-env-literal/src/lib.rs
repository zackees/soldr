#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_hir;

use dylint_linting::declare_late_lint;
use rustc_ast::LitKind;
use rustc_hir::{def::Res, Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass};

declare_late_lint! {
    /// ### What it does
    ///
    /// Rejects string literals beginning with `RUNNING_PROCESS_` when they are
    /// passed to the standard environment-variable APIs.
    ///
    /// ### Why is this bad?
    ///
    /// Broker escape hatches and test seams are process-safety boundaries.
    /// Repeating their names as literals makes typoed, stale, or inconsistently
    /// handled controls possible. The production crates define canonical
    /// constants for these names; callers should use those constants.
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// let disabled = std::env::var_os("RUNNING_PROCESS_DISABLE").is_some();
    /// ```
    ///
    /// Use instead:
    ///
    /// ```rust,ignore
    /// let disabled = std::env::var_os(RUNNING_PROCESS_DISABLE_ENV).is_some();
    /// ```
    pub RUNNING_PROCESS_ENV_LITERAL,
    Deny,
    "running-process environment controls must use canonical constants"
}

impl<'tcx> LateLintPass<'tcx> for RunningProcessEnvLiteral {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &Expr<'tcx>) {
        let ExprKind::Call(callee, arguments) = expr.kind else {
            return;
        };
        let ExprKind::Path(ref qpath) = callee.kind else {
            return;
        };
        let Res::Def(_, def_id) = cx.qpath_res(qpath, callee.hir_id) else {
            return;
        };
        if !matches!(
            cx.tcx.def_path_str(def_id).as_str(),
            "std::env::var" | "std::env::var_os" | "std::env::set_var" | "std::env::remove_var"
        ) {
            return;
        }
        let Some(argument) = arguments.first() else {
            return;
        };
        let ExprKind::Lit(literal) = argument.kind else {
            return;
        };
        let LitKind::Str(value, _) = literal.node else {
            return;
        };
        if !value.as_str().starts_with("RUNNING_PROCESS_") {
            return;
        }

        cx.tcx.dcx().span_err(
            argument.span,
            "use the canonical constant for this RUNNING_PROCESS_* environment control",
        );
    }
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
