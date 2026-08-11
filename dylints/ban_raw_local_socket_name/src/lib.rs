#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_hir::def::Res;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_span::{symbol::Symbol, FileName, RemapPathScopeComponents};

dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// Rejects direct conversion of local-socket display paths into
    /// `interprocess` names in Soldr production code. All bind and dial paths
    /// must share running-process's canonical conversion boundary so Windows
    /// `\\.\pipe\...` paths cannot acquire the namespace prefix twice.
    pub BAN_RAW_LOCAL_SOCKET_NAME,
    Deny,
    "require the canonical running-process local-socket name adapter"
}

impl<'tcx> LateLintPass<'tcx> for BanRawLocalSocketName {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if !span_is_in_scope(cx, expr.span) {
            return;
        }
        let def_id = match expr.kind {
            ExprKind::MethodCall(..) => cx.typeck_results().type_dependent_def_id(expr.hir_id),
            ExprKind::Path(qpath) => match cx.qpath_res(&qpath, expr.hir_id) {
                Res::Def(_, def_id) => Some(def_id),
                _ => None,
            },
            _ => None,
        };
        let Some(def_id) = def_id else {
            return;
        };
        let path = cx.get_def_path(def_id);
        let Some(method) = path.last().map(Symbol::as_str) else {
            return;
        };
        if !matches!(method, "to_ns_name" | "to_fs_name")
            || !path.iter().any(|part| *part == Symbol::intern("interprocess"))
        {
            return;
        }

        cx.opt_span_lint(
            BAN_RAW_LOCAL_SOCKET_NAME,
            Some(expr.span),
            DiagDecorator(move |diag| {
                diag.primary_message(format!(
                    "interprocess `{method}` can normalize an endpoint differently at bind and dial; use `running_process::broker::server::singleton_bind::wrap_socket_name`"
                ));
            }),
        );
    }
}

fn span_is_in_scope(cx: &LateContext<'_>, span: rustc_span::Span) -> bool {
    let filename = source_filename(cx, span).replace('\\', "/");
    if filename.starts_with("ui/") || filename.contains("/ui/") {
        return true;
    }
    let is_soldr = filename.contains("/crates/soldr-cli/src/")
        || filename.starts_with("crates/soldr-cli/src/")
        || filename.contains("/crates/soldr-daemon/src/")
        || filename.starts_with("crates/soldr-daemon/src/");
    let is_running_process = filename.contains("/_vender/running-process/crates/running-process/src/")
        || filename.starts_with("_vender/running-process/crates/running-process/src/");
    let is_canonical_boundary = filename.ends_with("/broker/server/singleton_bind.rs");
    is_soldr || (is_running_process && !is_canonical_boundary)
}

fn source_filename(cx: &LateContext<'_>, span: rustc_span::Span) -> String {
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
