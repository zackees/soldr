#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_hir::def::Res;
use rustc_hir::{Expr, ExprField, ExprKind, QPath, StructTailExpr};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_span::{symbol::Symbol, FileName, RemapPathScopeComponents};

dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// Rejects production code that discards a child process's stdout or
    /// stderr by wiring it to a null sink (soldr#3387, meta soldr#3389):
    ///
    /// * `Command::stdout(Stdio::null())` / `Command::stderr(Stdio::null())`
    ///   on `std::process::Command` or `tokio::process::Command`;
    /// * `running_process::SpawnStdio { stdout: StdioSource::Null, .. }` (or
    ///   `stderr`), and the `DaemonStdio` / `DaemonStdioSource` equivalent.
    ///
    /// `stdin` is never matched: closing a child's input is not swallowing
    /// its diagnostics.
    ///
    /// ### Why
    ///
    /// Owner directive: "We should always handle stdout, not just swallow
    /// it, and stderr as well." Soldr's contract for small tools (see
    /// `soldr_core::core::tool_output`) is **always forward + always log**:
    /// captured stderr is forwarded to Soldr's own stderr with a tool
    /// prefix, every run is recorded to `<soldr root>/logs/small-tools.jsonl`
    /// (argv, exit status, duration, bounded stdout/stderr), and a failure's
    /// error carries a stderr excerpt. A null sink makes all three
    /// impossible; route the call through `tool_output` instead.
    ///
    /// ### Escape hatch
    ///
    /// A site that is genuinely fire-and-forget (a detached process whose
    /// output is irrelevant, a probe whose only signal is existence) opts
    /// out explicitly, with the reason beside it:
    ///
    /// ```ignore
    /// // reason: <why discarding this output is correct>
    /// #[cfg_attr(dylint_lib = "ban_swallowed_child_stdio", allow(ban_swallowed_child_stdio))]
    /// ```
    ///
    /// ### Limits
    ///
    /// Test crates (`#[cfg(test)]` code, `tests/` targets) and examples are
    /// out of scope. The lint cannot see output that is captured and then
    /// dropped (`.output().ok()?`), a `Stdio::null()` bound to a local
    /// first, or Python/shell code.
    pub BAN_SWALLOWED_CHILD_STDIO,
    Deny,
    "child stdout/stderr discarded to a null sink"
}

const COMMAND_TYPES: &[&[&str]] = &[
    &["std", "process", "Command"],
    &["tokio", "process", "Command"],
];

const SPAWN_STDIO_STRUCTS: &[&str] = &["SpawnStdio", "DaemonStdio"];
const NULL_SOURCES: &[&[&str]] = &[
    &["StdioSource", "Null"],
    &["DaemonStdioSource", "Null"],
];
const OUTPUT_STREAMS: &[&str] = &["stdout", "stderr"];

impl<'tcx> LateLintPass<'tcx> for BanSwallowedChildStdio {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if !span_is_in_scope(cx, expr.span) {
            return;
        }
        match expr.kind {
            ExprKind::MethodCall(segment, _receiver, args, _) => {
                let stream = segment.ident.name.as_str();
                if !OUTPUT_STREAMS.contains(&stream) || args.len() != 1 {
                    return;
                }
                let Some(def_id) = cx.typeck_results().type_dependent_def_id(expr.hir_id) else {
                    return;
                };
                let path = cx.get_def_path(def_id);
                let is_command_method = COMMAND_TYPES.iter().any(|ty| {
                    let mut expected: Vec<&str> = ty.to_vec();
                    expected.push(stream);
                    path_ends_with(&path, &expected)
                });
                if is_command_method && is_stdio_null_call(cx, &args[0]) {
                    emit(
                        cx,
                        expr.span,
                        format!(
                            "`Command::{stream}(Stdio::null())` swallows the child's {stream}; \
                             capture it and forward + log it through \
                             `soldr_core::core::tool_output`, or add \
                             `#[cfg_attr(dylint_lib = \"ban_swallowed_child_stdio\", \
                             allow(ban_swallowed_child_stdio))]` with a `// reason:` comment"
                        ),
                    );
                }
            }
            ExprKind::Struct(qpath, fields, tail) => {
                check_spawn_stdio_struct(cx, expr, qpath, fields, tail);
            }
            _ => {}
        }
    }
}

fn check_spawn_stdio_struct<'tcx>(
    cx: &LateContext<'tcx>,
    expr: &'tcx Expr<'tcx>,
    qpath: &QPath<'tcx>,
    fields: &'tcx [ExprField<'tcx>],
    _tail: StructTailExpr<'tcx>,
) {
    let Res::Def(_, def_id) = cx.qpath_res(qpath, expr.hir_id) else {
        return;
    };
    let path = cx.get_def_path(def_id);
    let Some(last) = path.last() else {
        return;
    };
    if !SPAWN_STDIO_STRUCTS.contains(&last.as_str()) {
        return;
    }
    for field in fields {
        let name = field.ident.name.as_str();
        if !OUTPUT_STREAMS.contains(&name) {
            continue;
        }
        if is_null_source(cx, field.expr) {
            emit(
                cx,
                field.span,
                format!(
                    "`{}::{name}: Null` swallows the child's {name}; use a pipe or log file \
                     and forward + log it (soldr_core::core::tool_output), or add \
                     `#[cfg_attr(dylint_lib = \"ban_swallowed_child_stdio\", \
                     allow(ban_swallowed_child_stdio))]` with a `// reason:` comment",
                    last.as_str()
                ),
            );
        }
    }
}

fn is_stdio_null_call(cx: &LateContext<'_>, arg: &Expr<'_>) -> bool {
    let ExprKind::Call(callee, call_args) = arg.kind else {
        return false;
    };
    if !call_args.is_empty() {
        return false;
    }
    let ExprKind::Path(qpath) = callee.kind else {
        return false;
    };
    let Res::Def(_, def_id) = cx.qpath_res(&qpath, callee.hir_id) else {
        return false;
    };
    path_ends_with(&cx.get_def_path(def_id), &["process", "Stdio", "null"])
}

fn is_null_source(cx: &LateContext<'_>, value: &Expr<'_>) -> bool {
    let ExprKind::Path(qpath) = value.kind else {
        return false;
    };
    let Res::Def(_, def_id) = cx.qpath_res(&qpath, value.hir_id) else {
        return false;
    };
    let path = cx.get_def_path(def_id);
    NULL_SOURCES
        .iter()
        .any(|expected| path_ends_with(&path, expected))
}

fn emit(cx: &LateContext<'_>, span: rustc_span::Span, message: String) {
    cx.opt_span_lint(
        BAN_SWALLOWED_CHILD_STDIO,
        Some(span),
        DiagDecorator(move |diag| {
            diag.primary_message(message);
        }),
    );
}

/// Production sources only: UI fixtures, plus `crates/**/src/**` compiled as
/// a non-test crate. `#[cfg(test)]` code exists only in test crates, so the
/// test-crate check excludes it along with `tests/` integration targets.
fn span_is_in_scope(cx: &LateContext<'_>, span: rustc_span::Span) -> bool {
    let filename = source_filename(cx, span);
    let normalized = filename.replace('\\', "/");
    let is_ui_fixture = normalized.starts_with("ui/") || normalized.contains("/ui/");
    if is_ui_fixture {
        return true;
    }
    if cx.sess().is_test_crate() {
        return false;
    }
    let is_crate_source =
        normalized.contains("/crates/") || normalized.starts_with("crates/");
    let in_src = normalized.contains("/src/");
    let is_test_fixture = normalized.contains("/tests/") || normalized.contains("/examples/");
    is_crate_source && in_src && !is_test_fixture
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
