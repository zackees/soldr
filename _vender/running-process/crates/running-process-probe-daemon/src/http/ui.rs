//! The browsable UI, embedded in the binary (S13 / #642).
//!
//! Every asset is `include_str!`d rather than read from disk. Two reasons:
//! the daemon is a single binary an operator may have copied onto a host by
//! itself, so there is no assets directory to find; and an assets directory
//! would be a path the daemon reads at request time, which is a traversal
//! surface this way simply does not have.
//!
//! Nothing here loads from an external host. The page that helps you debug a
//! machine has to work on a machine with no network — and a diagnostic UI
//! phoning a CDN would leak that you are debugging, and what, to whoever
//! serves it.

use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};

/// The landing page.
const INDEX_HTML: &str = include_str!("ui/index.html");

/// Stylesheet.
const PROBE_CSS: &str = include_str!("ui/probe.css");

/// Application script, including the flame-graph renderer.
const PROBE_JS: &str = include_str!("ui/probe.js");

/// `GET /` — the UI.
///
/// Token-gated like every other route: the middleware has already run, so
/// reaching this function means the caller authenticated.
pub async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// `GET /assets/{file}` — one embedded asset.
///
/// Matched against a fixed list, not looked up. The parameter selects among
/// known constants; it never becomes part of a path.
pub async fn asset(Path(file): Path<String>) -> Response {
    let (body, content_type) = match file.as_str() {
        "probe.css" => (PROBE_CSS, "text/css; charset=utf-8"),
        "probe.js" => (PROBE_JS, "text/javascript; charset=utf-8"),
        _ => return (StatusCode::NOT_FOUND, "no such asset").into_response(),
    };
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UI must not reference any host but the daemon.
    ///
    /// Asserted on the asset text rather than trusted to review: a stray
    /// `<script src="https://cdn...">` added later would work perfectly on
    /// the author's laptop and fail on every air-gapped host the tool exists
    /// to help with.
    #[test]
    fn no_asset_references_an_external_host() {
        for (name, body) in [
            ("index.html", INDEX_HTML),
            ("probe.css", PROBE_CSS),
            ("probe.js", PROBE_JS),
        ] {
            for needle in ["http://", "https://", "//cdn.", "@import url("] {
                assert!(
                    !body.contains(needle),
                    "{name} references an external resource ({needle}); UI assets must be \
                     self-contained so the surface works on a host with no network"
                );
            }
        }
    }

    #[test]
    fn the_page_loads_the_assets_this_module_actually_serves() {
        assert!(INDEX_HTML.contains("/assets/probe.css"));
        assert!(INDEX_HTML.contains("/assets/probe.js"));
    }

    #[tokio::test]
    async fn an_unknown_asset_is_not_found() {
        let response = asset(Path("../../etc/passwd".to_string())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_known_asset_is_served_with_its_content_type() {
        let response = asset(Path("probe.js".to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/javascript; charset=utf-8")
        );
    }
}
