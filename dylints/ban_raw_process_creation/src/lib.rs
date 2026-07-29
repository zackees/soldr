#![feature(rustc_private)]

extern crate rustc_hir;
extern crate rustc_span;

use rustc_hir::def::Res;
use rustc_hir::{Expr, ExprKind, Item};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_span::{symbol::Symbol, FileName, RemapPathScopeComponents};

dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// Rejects production child-process execution in Soldr's daemon crate
    /// unless it goes through `running-process`.
    ///
    /// Constructing and configuring `std::process::Command` remains allowed.
    /// Executing it directly, setting Windows creation flags, or calling raw
    /// platform process-creation APIs does not.
    pub BAN_RAW_PROCESS_CREATION,
    Deny,
    "require running-process as the daemon process-creation boundary"
}

const BANNED_METHOD_SUFFIXES: &[&[&str]] = &[
    &["std", "process", "Command", "spawn"],
    &["std", "process", "Command", "output"],
    &["std", "process", "Command", "status"],
    &["tokio", "process", "Command", "spawn"],
    &["tokio", "process", "Command", "output"],
    &["tokio", "process", "Command", "status"],
];

const RAW_PROCESS_FUNCTIONS: &[&str] = &[
    "CreateProcessA",
    "CreateProcessW",
    "CreateProcessAsUserA",
    "CreateProcessAsUserW",
    "CreateProcessWithLogonW",
    "CreateProcessWithTokenW",
    "posix_spawn",
    "posix_spawnp",
    "fork",
    "vfork",
    "execv",
    "execve",
    "execvp",
    "execvpe",
    "execl",
    "execlp",
    "execle",
];

impl<'tcx> LateLintPass<'tcx> for BanRawProcessCreation {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if !span_is_in_scope(cx, expr.span) {
            return;
        }

        match expr.kind {
            ExprKind::MethodCall(..) => {
                let Some(def_id) = cx.typeck_results().type_dependent_def_id(expr.hir_id) else {
                    return;
                };
                check_resolved_path(cx, expr.span, def_id);
            }
            // Associated functions can be invoked with UFCS or stored as
            // function items. Resolved paths cover both forms, while a
            // Call-only matcher can be bypassed mechanically.
            ExprKind::Path(qpath) => {
                let Res::Def(_, def_id) = cx.qpath_res(&qpath, expr.hir_id) else {
                    return;
                };
                check_resolved_path(cx, expr.span, def_id);
            }
            _ => {}
        }
    }

    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        if !span_is_in_scope(cx, item.span) {
            return;
        }
        let Ok(snippet) = cx.sess().source_map().span_to_snippet(item.span) else {
            return;
        };
        if !snippet.contains("extern ") {
            return;
        }
        for name in RAW_PROCESS_FUNCTIONS {
            if snippet.contains(&format!("fn {name}")) {
                emit_raw_platform(cx, item.span, name);
                return;
            }
        }
    }
}

fn check_resolved_path(
    cx: &LateContext<'_>,
    span: rustc_span::Span,
    def_id: rustc_hir::def_id::DefId,
) {
    let path = cx.get_def_path(def_id);
    if let Some(banned) = BANNED_METHOD_SUFFIXES
        .iter()
        .find(|banned| path_ends_with(&path, banned))
    {
        emit(
            cx,
            span,
            format!(
                "`{}` bypasses running-process; configure the command, then execute it with \
                 `running_process::spawn` or `running_process::spawn_daemon*`",
                banned.join("::")
            ),
        );
        return;
    }

    if path.last() == Some(&Symbol::intern("creation_flags")) {
        emit(
            cx,
            span,
            "`CommandExt::creation_flags` duplicates running-process's Windows console and \
             containment policy; remove the flag and execute through running-process"
                .to_string(),
        );
        return;
    }

    let Some(name) = path.last().map(Symbol::as_str) else {
        return;
    };
    if RAW_PROCESS_FUNCTIONS.contains(&name) {
        emit_raw_platform(cx, span, &name);
    }
}

fn emit_raw_platform(cx: &LateContext<'_>, span: rustc_span::Span, name: &str) {
    emit(
        cx,
        span,
        format!(
            "raw platform process API `{name}` bypasses running-process; remove the declaration or \
             call and use `running_process::spawn` or `running_process::spawn_daemon*`"
        ),
    );
}

fn emit(cx: &LateContext<'_>, span: rustc_span::Span, message: String) {
    cx.span_lint(BAN_RAW_PROCESS_CREATION, span, move |diag| {
        diag.primary_message(message);
    });
}

fn span_is_in_scope(cx: &LateContext<'_>, span: rustc_span::Span) -> bool {
    let filename = source_filename(cx, span);
    let normalized = filename.replace('\\', "/");
    let is_ui_fixture = normalized.starts_with("ui/") || normalized.contains("/ui/");
    let is_daemon_source = normalized.contains("/crates/soldr-daemon/src/")
        || normalized.starts_with("crates/soldr-daemon/src/");
    let is_test_fixture =
        normalized.contains("/crates/soldr-daemon/src/") && normalized.contains("/tests/");
    is_ui_fixture || (is_daemon_source && !is_test_fixture)
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

fn path_ends_with(path: &[Symbol], expected: &[&str]) -> bool {
    path.len() >= expected.len()
        && path[path.len() - expected.len()..]
            .iter()
            .zip(expected)
            .all(|(actual, expected)| *actual == Symbol::intern(expected))
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
