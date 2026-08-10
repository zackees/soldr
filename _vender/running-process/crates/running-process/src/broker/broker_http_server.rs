//! Broker HTTP server scaffold (slice 7 of #488).
//!
//! Tiny single-threaded HTTP/1.1 server using only `std::net::TcpListener`
//! — no hyper/axum dep yet, just enough to bind a port, accept a request,
//! and respond with a placeholder page that lists the currently-registered
//! backends from [`super::http_endpoint_registry::HttpEndpointRegistry`].
//!
//! Honors the resolved bind state from
//! [`super::broker_http_port::BrokerHttpPort::resolve`]: the port is one of
//! `Static`, `Dynamic`, or `StaticOrFallback`; the address comes from the
//! env override or defaults to `127.0.0.1`.
//!
//! The aggregator iframe page lands in slice 8. This slice produces only a
//! plain-text list so consumers can verify the server is reachable and the
//! registry is wired correctly.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;

use crate::broker::broker_http_port::{BrokerHttpPort, ResolvedHttpBind};
use crate::broker::http_endpoint_registry::HttpEndpointRegistry;

/// Errors raised by `BrokerHttpServer::bind`.
#[derive(Debug, thiserror::Error)]
pub enum BrokerHttpServerError {
    /// `bind(addr:port)` failed and we have no fallback to fall back to.
    #[error("bind {addr}:{port} failed: {source}")]
    Bind {
        /// IP we tried to bind on.
        addr: std::net::IpAddr,
        /// Port we tried to bind on.
        port: u16,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
}

/// A bound but not-yet-serving HTTP listener. Caller decides whether to
/// drive `serve_once` in a blocking thread, behind tokio, etc.
pub struct BrokerHttpServer {
    listener: TcpListener,
    local: SocketAddr,
    registry: Arc<HttpEndpointRegistry>,
}

impl BrokerHttpServer {
    /// Resolve the [`BrokerHttpPort`] config + env, then bind a
    /// `TcpListener` on the resulting address.
    ///
    /// Behavior per #483 §3:
    /// - `Static`: bind exactly that port; bubble up the bind error.
    /// - `Dynamic`: bind to `port=0` (OS-allocated).
    /// - `StaticOrFallback`: try the preferred port; on EADDRINUSE
    ///   retry with `port=0`.
    pub fn bind(
        config: BrokerHttpPort,
        registry: Arc<HttpEndpointRegistry>,
    ) -> Result<Self, BrokerHttpServerError> {
        let resolved = BrokerHttpPort::resolve(config);
        let listener = match resolved.port {
            BrokerHttpPort::Static { port } => try_bind(resolved, port)?,
            BrokerHttpPort::Dynamic => try_bind(resolved, 0)?,
            BrokerHttpPort::StaticOrFallback { preferred } => match try_bind(resolved, preferred) {
                Ok(l) => l,
                Err(BrokerHttpServerError::Bind { source, .. })
                    if source.kind() == std::io::ErrorKind::AddrInUse =>
                {
                    try_bind(resolved, 0)?
                }
                Err(other) => return Err(other),
            },
        };
        let local = listener
            .local_addr()
            .map_err(|source| BrokerHttpServerError::Bind {
                addr: resolved.addr,
                port: 0,
                source,
            })?;
        Ok(Self {
            listener,
            local,
            registry,
        })
    }

    /// The actual bound `SocketAddr` (post-resolution). Use this to
    /// populate `GetBrokerHttpEndpointResponse.port` and the runtime-file
    /// shape (slice 9 plumbs the resolved address through).
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Accept ONE connection and respond with the placeholder page,
    /// then return. Intended for tests + the future slice-7 serve loop.
    pub fn serve_once(&self) -> std::io::Result<()> {
        let (stream, _peer) = self.listener.accept()?;
        handle_one(stream, &self.registry)
    }
}

fn try_bind(resolved: ResolvedHttpBind, port: u16) -> Result<TcpListener, BrokerHttpServerError> {
    let bind_addr = SocketAddr::new(resolved.addr, port);
    TcpListener::bind(bind_addr).map_err(|source| BrokerHttpServerError::Bind {
        addr: resolved.addr,
        port,
        source,
    })
}

fn handle_one(mut stream: TcpStream, registry: &HttpEndpointRegistry) -> std::io::Result<()> {
    // Minimal HTTP/1.1: read until "\r\n\r\n", grab the request line,
    // route GET / to the placeholder page, fall through to 404.
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    let _ = reader.read_line(&mut request_line);
    let mut headers_done = false;
    while !headers_done {
        let mut buf = [0u8; 1];
        if reader.read(&mut buf)? == 0 {
            break;
        }
        if buf[0] == b'\r' {
            let mut peek = [0u8; 3];
            let n = reader.read(&mut peek)?;
            if n >= 3 && peek == [b'\n', b'\r', b'\n'] {
                headers_done = true;
            }
        }
        // The placeholder server does not consume request bodies; we
        // assume the client is a no-body GET.
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();

    let (status_line, content_type, body) =
        if request_line.starts_with("GET ") && (path == "/" || path.is_empty()) {
            (
                "HTTP/1.1 200 OK",
                "text/html; charset=utf-8",
                render_aggregator_page(registry),
            )
        } else {
            (
                "HTTP/1.1 404 Not Found",
                "text/plain; charset=utf-8",
                "not found\n".to_string(),
            )
        };

    let response = format!(
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Render the aggregator page (slice 8 of #488): top-bar selector +
/// full-bleed iframe. The selector emits one button per registered
/// backend; clicking flips the iframe's `src` to that backend's HTTP
/// root. Backends whose registry slot is `None` render as disabled
/// buttons with `(starting…)` text — they don't accidentally try to
/// load a URL the broker doesn't have yet.
///
/// The page is a single self-contained document: no external CSS,
/// no external JS, no fonts. Keeps it loadable on locked-down
/// operator boxes and trivially auditable.
fn render_aggregator_page(registry: &HttpEndpointRegistry) -> String {
    let mut snap = registry.snapshot();
    snap.sort_by(|a, b| a.0.cmp(&b.0));

    let mut buttons = String::new();
    let mut initial_src = String::new();
    if snap.is_empty() {
        buttons.push_str(r#"<span class="empty">no backends registered yet</span>"#);
    } else {
        for (id, port) in &snap {
            match port {
                Some(p) => {
                    let url = format!("http://127.0.0.1:{p}/");
                    if initial_src.is_empty() {
                        initial_src.clone_from(&url);
                    }
                    buttons.push_str(&format!(
                        r#"<button onclick="document.getElementById('agg').src={url:?}">{}</button>"#,
                        html_escape(id),
                    ));
                }
                None => {
                    buttons.push_str(&format!(
                        r#"<button disabled title="backend has not reported a port yet">{} (starting…)</button>"#,
                        html_escape(id),
                    ));
                }
            }
        }
    }

    let initial_src_attr = if initial_src.is_empty() {
        "about:blank".to_string()
    } else {
        initial_src
    };

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>running-process broker-v2 aggregator</title>
<style>
  html, body {{ margin: 0; padding: 0; height: 100%; font-family: system-ui, sans-serif; }}
  #bar {{ display: flex; gap: 0.4rem; padding: 0.4rem; border-bottom: 1px solid #ccc; background: #f5f5f5; }}
  #bar button {{ padding: 0.3rem 0.8rem; }}
  #agg {{ width: 100%; height: calc(100% - 3rem); border: 0; }}
  .empty {{ color: #888; font-style: italic; }}
</style>
</head>
<body>
<nav id="bar">{buttons}</nav>
<iframe id="agg" src="{initial_src_attr}"></iframe>
</body>
</html>
"#
    )
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    fn make_server() -> BrokerHttpServer {
        let reg = Arc::new(HttpEndpointRegistry::new());
        reg.track("zccache".to_string());
        reg.register_backend_http_endpoint("fbuild".to_string(), 8002);
        BrokerHttpServer::bind(BrokerHttpPort::Dynamic, reg).expect("dynamic bind succeeds")
    }

    #[test]
    fn dynamic_bind_yields_nonzero_port() {
        let s = make_server();
        let addr = s.local_addr();
        assert_ne!(addr.port(), 0, "OS should have assigned a real port");
    }

    #[test]
    fn placeholder_renders_registered_backends() {
        let s = make_server();
        let local = s.local_addr();
        let handle = thread::spawn(move || {
            s.serve_once().expect("serve_once succeeds");
        });
        // Hit the server with a minimal HTTP GET.
        let mut client = TcpStream::connect(local).expect("connect");
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("write request");
        client
            // A harness bound on how long to wait for the answer, not the
            // property under test — that is the page content asserted below.
            // Two seconds is enough on an idle machine and not enough on a
            // busy one: the suite runs these in parallel, and a test that
            // spawns processes elsewhere can push this past the limit. A
            // response that is merely slow is not a defect, so the bound is
            // generous; a server that never answers still fails.
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("set_read_timeout");
        let mut buf = String::new();
        client.read_to_string(&mut buf).expect("read response");

        assert!(
            buf.contains("200 OK"),
            "expected 200 OK in response, got:\n{buf}"
        );
        assert!(
            buf.contains("text/html"),
            "expected HTML content-type, got:\n{buf}"
        );
        assert!(
            buf.contains("<iframe id=\"agg\""),
            "expected aggregator iframe element, got:\n{buf}"
        );
        assert!(
            buf.contains("http://127.0.0.1:8002/"),
            "expected fbuild URL wired into selector, got:\n{buf}"
        );
        assert!(
            buf.contains("zccache (starting"),
            "expected zccache pending-state button, got:\n{buf}"
        );
        assert!(
            buf.contains("src=\"http://127.0.0.1:8002/\""),
            "expected fbuild URL as initial iframe src, got:\n{buf}"
        );

        handle.join().expect("server thread joins");
    }

    #[test]
    fn aggregator_page_with_no_backends_shows_empty_state() {
        let reg = Arc::new(HttpEndpointRegistry::new());
        let s =
            BrokerHttpServer::bind(BrokerHttpPort::Dynamic, reg).expect("dynamic bind succeeds");
        let local = s.local_addr();
        let handle = thread::spawn(move || {
            s.serve_once().expect("serve_once succeeds");
        });
        let mut client = TcpStream::connect(local).expect("connect");
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("write request");
        client
            // A harness bound on how long to wait for the answer, not the
            // property under test — that is the page content asserted below.
            // Two seconds is enough on an idle machine and not enough on a
            // busy one: the suite runs these in parallel, and a test that
            // spawns processes elsewhere can push this past the limit. A
            // response that is merely slow is not a defect, so the bound is
            // generous; a server that never answers still fails.
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("set_read_timeout");
        let mut buf = String::new();
        client.read_to_string(&mut buf).expect("read response");

        assert!(buf.contains("no backends registered yet"), "got:\n{buf}");
        assert!(
            buf.contains("src=\"about:blank\""),
            "empty selector should default the iframe to about:blank, got:\n{buf}"
        );
        handle.join().expect("server thread joins");
    }

    #[test]
    fn static_or_fallback_falls_back_on_eaddrinuse() {
        // Bind a sacrificial listener to force EADDRINUSE on its port.
        let blocker = TcpListener::bind("127.0.0.1:0").expect("blocker bind");
        let preferred = blocker.local_addr().expect("blocker addr").port();

        let reg = Arc::new(HttpEndpointRegistry::new());
        let s = BrokerHttpServer::bind(BrokerHttpPort::StaticOrFallback { preferred }, reg)
            .expect("StaticOrFallback should fall back to OS-allocated");
        let fallback_port = s.local_addr().port();
        assert_ne!(
            fallback_port, preferred,
            "StaticOrFallback should have picked a different port"
        );
        assert_ne!(fallback_port, 0, "OS should have assigned a real port");
        drop(blocker);
    }
}
