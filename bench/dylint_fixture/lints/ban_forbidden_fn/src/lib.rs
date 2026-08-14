#![feature(rustc_private)]

// No `extern crate rustc_lint;` here on purpose: declare_late_lint! emits one
// itself, and declaring it again is E0259 ("defined multiple times"). The
// `use rustc_lint::...` below still resolves through the macro's declaration.
// zccache's dylints do the same — they declare rustc_ast/errors/hir/span and
// deliberately omit rustc_lint.
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_hir::{Expr, ExprKind, QPath};
use rustc_lint::{LateContext, LateLintPass, LintContext};

// API shape (declare_late_lint! + a LateLintPass checking Expr nodes) is
// modeled directly on zccache's `dylints/ban_tmp_literal` (dylint_linting
// pinned via git rev there, matching a pre-release rustc_session fix; this
// fixture pins the released 6.0.1 crates.io version instead). The basic
// late-lint-pass API used here is stable across the 5.x -> 6.x range covered
// by both pins.
dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// Forbids calls to `dep_user::forbidden_marker_fn`.
    ///
    /// ### Why is this bad?
    ///
    /// This is a synthetic rule for the soldr#1788 dylint perf fixture: it
    /// exists purely so `bench/dylint_perf.py` has a real diagnostic to
    /// assert on in `--expect-fail` mode. There is no real-world badness
    /// here beyond "the fixture says so".
    ///
    /// ### Example
    ///
    /// ```rust
    /// dep_user::forbidden_marker_fn();
    /// ```
    ///
    /// Use instead: don't call it. (Fixture-only lint; see
    /// `app/src/violation.rs.disabled` for the trigger site.)
    pub BAN_FORBIDDEN_FN,
    Deny,
    "forbid calls to dep_user::forbidden_marker_fn (soldr#1788 dylint perf fixture)"
}

impl<'tcx> LateLintPass<'tcx> for BanForbiddenFn {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::Call(callee, _args) = expr.kind else {
            return;
        };
        let ExprKind::Path(QPath::Resolved(_, path)) = callee.kind else {
            return;
        };
        let Some(segment) = path.segments.last() else {
            return;
        };
        if segment.ident.name.as_str() != "forbidden_marker_fn" {
            return;
        }
        // rustc's late-lint emit API changed under the pinned Dylint nightly:
        // `LateContext::span_lint` was removed in favor of `opt_span_lint`
        // (Option<Span> + a `DiagDecorator`-wrapped closure). Mirrors the
        // in-repo `dylints/*` lints, which build against the same nightly.
        cx.opt_span_lint(
            BAN_FORBIDDEN_FN,
            Some(expr.span),
            DiagDecorator(move |diag| {
                diag.primary_message(
                    "call to `forbidden_marker_fn` is forbidden by the \
                     ban_forbidden_fn dylint fixture lint (soldr#1788)",
                );
            }),
        );
    }
}
