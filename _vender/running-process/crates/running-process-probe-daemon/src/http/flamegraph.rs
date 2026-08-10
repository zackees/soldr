//! The flame-graph page and profile downloads (S16 / #645).
//!
//! # One self-contained document
//!
//! The page embeds its own renderer *and* its own data. Nothing is fetched:
//! no CDN, no font, no second request for the profile JSON. That matters
//! because the machine you are profiling is disproportionately likely to be
//! one with no working network — and because a diagnostic page that phoned a
//! third party would disclose *that* you are debugging, and what, to whoever
//! served it.
//!
//! A strict `Content-Security-Policy` makes that structural rather than
//! aspirational: `default-src 'none'` means any external reference introduced
//! later fails loudly in the browser instead of silently working on the
//! author's laptop and nowhere else.
//!
//! # Renderer
//!
//! Hand-written, ~60 lines of DOM, rather than a vendored d3 build. A flame
//! graph is nested rectangles with proportional widths; the whole of what a
//! library buys here is zoom and a tooltip, and vendoring a few hundred
//! kilobytes of minified third-party JS to get them is a poor trade in a page
//! that must be auditable as self-contained.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::http::HttpState;
use crate::profile::export::{to_collapsed, to_firefox_json, to_pprof_gzip};
use crate::profile::store::{session_to_tree, FlameNode};

/// The renderer and styling for the flame-graph page.
const FLAME_JS: &str = include_str!("ui/flame.js");

/// Content-Security-Policy for the flame-graph page.
///
/// `default-src 'none'` is the load-bearing part: it forbids every fetch the
/// page might make, so an accidental external reference is a visible console
/// error rather than a silent dependency on someone else's server.
/// `'unsafe-inline'` is required precisely *because* everything is inlined —
/// there is no external file to point a hash or nonce at.
pub const FLAME_CSP: &str =
    "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:";

/// Which profile, and which weight column.
#[derive(Debug, Default, Deserialize)]
pub struct FlameQuery {
    /// A crash-store artifact id holding collapsed stacks, instead of a
    /// retained profile.
    pub artifact: Option<i64>,
}

/// `GET /v1/profiles/{id}/flamegraph` — the interactive page.
pub async fn page(State(state): State<HttpState>, Path(id): Path<u64>) -> Response {
    let Some(result) = state.profiles().get(id) else {
        return (StatusCode::NOT_FOUND, "no retained profile with that id").into_response();
    };
    let tree = session_to_tree(&result);
    let subtitle = format!(
        "{} samples, {} dropped, {:.0}% thread coverage, {:.2}% overhead",
        result.metrics.samples_captured,
        result.metrics.samples_dropped,
        result.metrics.thread_coverage() * 100.0,
        result.metrics.overhead_ratio() * 100.0,
    );
    render(&tree, &format!("profile {id}"), &subtitle)
}

/// `GET /v1/flame` — the tree as JSON, for the UI's own flame view.
pub async fn tree(
    State(state): State<HttpState>,
    Query(query): Query<FlameQuery>,
) -> Result<axum::Json<FlameNode>, (StatusCode, String)> {
    if let Some(artifact) = query.artifact {
        let store = state
            .ops()
            .crash_store()
            .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no artifact store".into()))?;
        let guard = store
            .begin_fetch(artifact)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
            .ok_or((StatusCode::NOT_FOUND, "no artifact with that id".into()))?;
        let text = std::fs::read_to_string(guard.path())
            .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
        return Ok(axum::Json(crate::profile::store::collapsed_to_tree(&text)));
    }

    // No artifact named: render the most recent retained profile, which is
    // what an operator who just clicked "profile" means.
    let Some(id) = state.profiles().ids().first().copied() else {
        return Err((
            StatusCode::NOT_FOUND,
            "no profile has been captured yet".into(),
        ));
    };
    let result = state
        .profiles()
        .get(id)
        .ok_or((StatusCode::NOT_FOUND, "profile expired".into()))?;
    Ok(axum::Json(session_to_tree(&result)))
}

/// `GET /v1/profiles/{id}.{format}` — download one export.
pub async fn download(
    State(state): State<HttpState>,
    Path((id, format)): Path<(u64, String)>,
) -> Response {
    let Some(result) = state.profiles().get(id) else {
        return (StatusCode::NOT_FOUND, "no retained profile with that id").into_response();
    };

    let (bytes, content_type, extension) = match format.as_str() {
        "pprof" | "pb.gz" => match to_pprof_gzip(&result) {
            Ok(bytes) => (bytes, "application/octet-stream", "pb.gz"),
            Err(error) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
            }
        },
        "json" => (
            to_firefox_json(&result).into_bytes(),
            "application/json",
            "json",
        ),
        "collapsed" => (
            to_collapsed(&result).into_bytes(),
            "text/plain; charset=utf-8",
            "collapsed",
        ),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                "unknown format; use pprof, json, or collapsed",
            )
                .into_response()
        }
    };

    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                // Built from the id and a fixed extension, never from stored
                // text, so nothing in a profile can inject a header.
                format!("attachment; filename=\"profile-{id}.{extension}\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Build the self-contained page.
pub fn render(tree: &FlameNode, title: &str, subtitle: &str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CONTENT_SECURITY_POLICY, FLAME_CSP.to_string()),
        ],
        render_html(tree, title, subtitle),
    )
        .into_response()
}

/// The page's markup.
///
/// Separate from [`render`] so the self-containment test can assert on the
/// bytes actually served rather than on a reconstruction of them — a test that
/// rebuilt the page from the same pieces would keep passing if the handler
/// stopped using them.
pub fn render_html(tree: &FlameNode, title: &str, subtitle: &str) -> String {
    let data = serde_json::to_string(tree).unwrap_or_else(|_| "null".to_string());
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title} — rpprobed</title>\
         <style>{}</style></head><body>\
         <header><h1>{title}</h1><p class=\"subtitle\">{subtitle}</p></header>\
         <p class=\"hint\">Click a frame to zoom in; click the header to reset.</p>\
         <div id=\"flame\"></div>\
         <script>const PROFILE = {data};</script>\
         <script>{FLAME_JS}</script></body></html>",
        flame_css(),
    )
}

/// Styling for the page. Inline, like everything else.
fn flame_css() -> &'static str {
    include_str!("ui/flame.css")
}
