#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_span;

use rustc_errors::DiagDecorator;
use rustc_hir::def::Res;
use rustc_hir::{Expr, ExprKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_span::{FileName, RemapPathScopeComponents};

dylint_linting::declare_late_lint! {
    /// ### What it does
    ///
    /// Rejects raw local-socket and named-pipe constructors outside Soldr's
    /// blessed IPC transport adapters.
    pub BAN_RAW_IPC_TRANSPORT,
    Deny,
    "require Soldr IPC construction to use blessed internal transport adapters"
}

impl<'tcx> LateLintPass<'tcx> for BanRawIpcTransport {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let filename = source_filename(cx, expr.span);
        if !filename_is_in_scope(&filename) || filename_is_blessed_adapter(&filename) {
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
        let path = cx
            .get_def_path(def_id)
            .into_iter()
            .map(|symbol| symbol.as_str().to_string())
            .collect::<Vec<_>>();
        let Some(api) = raw_ipc_api(&path) else {
            return;
        };

        cx.opt_span_lint(
            BAN_RAW_IPC_TRANSPORT,
            Some(expr.span),
            DiagDecorator(move |diag| {
                diag.primary_message(format!(
                    "raw IPC constructor `{api}` bypasses Soldr's blessed transport boundary; call the platform-neutral internal IPC facade"
                ));
            }),
        );
    }
}

fn raw_ipc_api(path: &[String]) -> Option<String> {
    let method = path.last()?.as_str();
    let has = |part: &str| path.iter().any(|item| item == part);

    let raw = (has("interprocess")
        && (((has("ListenerOptions") || has("ConnectOptions")) && method == "new")
            || (has("Stream") && method == "connect")
            || (has("DuplexPipeStream")
                && matches!(method, "connect" | "connect_by_path_with_wait_mode"))))
        || (has("std")
            && (has("UnixListener") || has("UnixStream") || has("UnixDatagram"))
            && matches!(method, "bind" | "connect"))
        || (has("tokio")
            && ((has("UnixListener") || has("UnixStream") || has("UnixDatagram"))
                && matches!(method, "bind" | "connect")
                || (has("ServerOptions") || has("ClientOptions")) && method == "new"));
    raw.then(|| path.join("::"))
}

fn filename_is_in_scope(filename: &str) -> bool {
    let filename = filename.replace('\\', "/");
    if filename.starts_with("ui/") || filename.contains("/ui/") {
        return true;
    }
    let is_production = (filename.contains("/crates/soldr-")
        || filename.starts_with("crates/soldr-"))
        && filename.contains("/src/");
    let is_test = filename.ends_with("/tests.rs") || filename.contains("/tests/");
    is_production && !is_test
}

fn filename_is_blessed_adapter(filename: &str) -> bool {
    let filename = filename.replace('\\', "/");
    let filename = format!("/{}", filename.trim_start_matches('/'));
    [
        "/crates/soldr-daemon/src/daemon/client.rs",
        "/crates/soldr-daemon/src/daemon/server.rs",
        "/crates/soldr-daemon/src/daemon/ipc_peer.rs",
        "/crates/soldr-daemon/src/daemon/session_endpoint.rs",
        "/crates/soldr-cli/src/broker_server.rs",
        "/crates/soldr-cli/src/broker_spawn.rs",
        "/crates/soldr-cli/src/broker_control_transport_unix.rs",
        "/crates/soldr-cli/src/broker_control_transport_windows.rs",
        "/crates/soldr-cli/src/session_transport.rs",
    ]
    .iter()
    .any(|suffix| filename.ends_with(suffix))
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

#[test]
fn classifies_supported_raw_constructor_families() {
    for path in [
        vec!["interprocess", "ListenerOptions", "new"],
        vec!["interprocess", "ConnectOptions", "new"],
        vec!["interprocess", "Stream", "connect"],
        vec!["interprocess", "DuplexPipeStream", "connect"],
        vec![
            "interprocess",
            "DuplexPipeStream",
            "connect_by_path_with_wait_mode",
        ],
        vec!["std", "UnixListener", "bind"],
        vec!["std", "UnixStream", "connect"],
        vec!["std", "UnixDatagram", "bind"],
        vec!["tokio", "UnixListener", "bind"],
        vec!["tokio", "UnixStream", "connect"],
        vec!["tokio", "UnixDatagram", "connect"],
        vec!["tokio", "ServerOptions", "new"],
        vec!["tokio", "ClientOptions", "new"],
    ] {
        let path = path.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert!(raw_ipc_api(&path).is_some(), "missed {path:?}");
    }
}

#[test]
fn blessed_adapter_scope_is_exact() {
    assert!(filename_is_in_scope(
        "/repo/crates/soldr-cache/src/future_ipc.rs"
    ));
    assert!(filename_is_blessed_adapter(
        "/repo/crates/soldr-daemon/src/daemon/session_endpoint.rs"
    ));
    assert!(filename_is_blessed_adapter(
        "crates/soldr-daemon/src/daemon/session_endpoint.rs"
    ));
    assert!(filename_is_blessed_adapter(
        "/repo/crates/soldr-cli/src/broker_control_transport_windows.rs"
    ));
    assert!(filename_is_blessed_adapter(
        r"crates\soldr-cli\src\broker_control_transport_windows.rs"
    ));
    assert!(!filename_is_blessed_adapter(
        "/repo/crates/soldr-cli/src/random_feature.rs"
    ));
    assert!(!filename_is_blessed_adapter(
        "/repo/crates/soldr-cache/src/daemon/session_endpoint.rs"
    ));
}
