//! Bearer-token authentication for the HTTP surface (S13 / #642).
//!
//! # Loopback is not an authorization boundary
//!
//! Binding to `127.0.0.1` keeps the surface off the network. It does *not*
//! keep it away from other users: on a shared host every local account can
//! reach loopback, and on Windows so can any process in any session. The
//! control socket has peer credentials to lean on; a TCP listener has
//! nothing. So the token is mandatory on **every** route, including the
//! landing page — there is no "just the UI" tier, because the UI is what
//! calls the API.
//!
//! # Where the token may travel
//!
//! Three carriers, because a browser cannot set a header on a navigation:
//!
//! - `Authorization: Bearer <token>` — what the UI's `fetch` calls use, and
//!   the only one a script should use.
//! - `?token=<token>` — the Jupyter-style URL the daemon prints at startup.
//!   Convenient, but it lands in shell history and server logs, so the
//!   landing page immediately trades it for the third carrier.
//! - a `probe_token` cookie, set by the landing page.
//!
//! Comparison is constant-time. A `==` on the token would return as soon as
//! two bytes differed, which over enough requests reveals the token one byte
//! at a time — and the token is the only thing between a local process and
//! every crash artifact this daemon holds.

use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

use crate::http::HttpState;

/// Cookie the landing page sets so later navigations need no query string.
pub const TOKEN_COOKIE: &str = "probe_token";

/// Query parameter carrying the token on a first navigation.
pub const TOKEN_QUERY: &str = "token";

/// Reject any request that does not carry the daemon's token.
pub async fn require_bearer(
    State(state): State<HttpState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    match presented_token(&request) {
        Some(presented) if token_matches(&presented, state.token()) => Ok(next.run(request).await),
        // 401 rather than 403 for both the missing and the wrong case: the
        // difference between "you did not authenticate" and "your token is
        // wrong" is itself information, and it is the only information an
        // unauthenticated caller is trying to get.
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Pull the token out of whichever carrier the caller used.
fn presented_token(request: &Request<axum::body::Body>) -> Option<String> {
    if let Some(value) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) = value.strip_prefix("Bearer ") {
            return Some(token.trim().to_string());
        }
    }

    if let Some(query) = request.uri().query() {
        if let Some(token) = query_param(query, TOKEN_QUERY) {
            return Some(token);
        }
    }

    request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| cookie_value(cookies, TOKEN_COOKIE))
}

/// Read one parameter out of a raw query string.
///
/// Hand-rolled rather than pulled from a form crate: the only value read here
/// is hex, so percent-decoding would be decoration, and a decoder is one more
/// place for a parsing difference to become an auth difference.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

/// Read one cookie out of a `Cookie:` header.
fn cookie_value(cookies: &str, name: &str) -> Option<String> {
    cookies.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

/// Compare two tokens without leaking where they first differ.
pub fn token_matches(presented: &str, expected: &str) -> bool {
    // Length is compared separately and is not itself secret — the token is a
    // fixed 64 hex chars, so an attacker already knows it.
    presented.len() == expected.len() && bool::from(presented.as_bytes().ct_eq(expected.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn an_exact_token_matches() {
        assert!(token_matches(TOKEN, TOKEN));
    }

    #[test]
    fn a_wrong_token_of_the_same_length_does_not_match() {
        let mut wrong = TOKEN.to_string();
        wrong.replace_range(63..64, "0");
        assert!(!token_matches(&wrong, TOKEN));
    }

    #[test]
    fn a_prefix_of_the_token_does_not_match() {
        // The failure mode a naive `starts_with` would have.
        assert!(!token_matches(&TOKEN[..32], TOKEN));
    }

    #[test]
    fn an_empty_token_does_not_match() {
        assert!(!token_matches("", TOKEN));
    }

    #[test]
    fn a_query_parameter_is_read_by_name_not_by_position() {
        assert_eq!(
            query_param("a=1&token=xyz&b=2", "token").as_deref(),
            Some("xyz")
        );
        assert_eq!(query_param("nottoken=xyz", "token"), None);
        assert_eq!(query_param("", "token"), None);
    }

    #[test]
    fn a_cookie_is_read_by_name_and_trimmed() {
        assert_eq!(
            cookie_value("other=1; probe_token=xyz; more=2", TOKEN_COOKIE).as_deref(),
            Some("xyz")
        );
        // A cookie whose name merely ends with the token name must not match.
        assert_eq!(cookie_value("xprobe_token=xyz", TOKEN_COOKIE), None);
    }
}
