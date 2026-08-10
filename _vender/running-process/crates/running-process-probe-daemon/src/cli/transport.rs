//! How `rpprobe` reaches the daemon (S14 / #643).
//!
//! Two transports, and the choice between them is not a preference:
//!
//! - **Control socket.** Authorized by *peer credentials* — the OS tells the
//!   daemon who connected, and nothing the client sends can change that
//!   answer. Nothing is transmitted that would be worth stealing.
//! - **HTTP.** Authorized by a bearer token read out of the owner-only
//!   discovery file. That is a secret in flight, and a secret in flight is a
//!   secret that can leak.
//!
//! So the socket is the default and HTTP is the fallback, not the other way
//! round. `--http` forces the fallback for the case the socket cannot serve:
//! an artifact larger than the 16 MiB frame cap.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use prost::Message as _;
use running_process::broker::protocol::framing::{read_frame_with_cap, write_frame};
use running_process_probe::probe_diag::v1::{probe_envelope::Body, ProbeEnvelope};

use crate::discovery::{discovery_dir, DiscoveryInfo, DISCOVERY_FILE};

/// Override for the discovery file location, so a test can run its own daemon.
pub const DISCOVERY_ENV: &str = "RUNNING_PROCESS_PROBE_DISCOVERY";

/// Cap on one reply frame, matching the daemon's request cap.
const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;

/// How long to wait for the daemon to answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a command could not reach or complete against the daemon.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// No discovery file, so no daemon has published itself.
    #[error(
        "no probe daemon found (looked for {path}). Start one with `rpprobed`, \
         or point {DISCOVERY_ENV} at its runtime directory."
    )]
    NoDaemon {
        /// Where the discovery file was expected.
        path: PathBuf,
    },
    /// The discovery file exists but does not parse.
    #[error("discovery file {path} is unreadable: {detail}")]
    BadDiscovery {
        /// The offending file.
        path: PathBuf,
        /// Parser detail.
        detail: String,
    },
    /// The daemon is published but not answering.
    #[error("cannot reach the probe daemon: {0}")]
    Unreachable(io::Error),
    /// Transport failure mid-request.
    #[error("probe daemon request failed: {0}")]
    Io(#[from] io::Error),
    /// The framed transport failed, including an oversize reply.
    ///
    /// Its own variant rather than folded into `Io`: an over-cap reply is a
    /// real and actionable case — the answer did not fit the socket, and the
    /// caller should be fetching it over HTTP instead.
    #[error("probe daemon framing error: {0}")]
    Framing(#[from] running_process::broker::protocol::framing::FramingError),
    /// The daemon replied with something this build cannot interpret.
    #[error("probe daemon sent an unexpected reply: {0}")]
    UnexpectedReply(String),
    /// The daemon refused the request, with its own reason.
    #[error("probe daemon refused the request: {0}")]
    Refused(String),
}

/// Locate and read the discovery file.
pub fn load_discovery(explicit: Option<&Path>) -> Result<(PathBuf, DiscoveryInfo), CliError> {
    let path = match explicit {
        Some(path) if path.is_dir() => path.join(DISCOVERY_FILE),
        Some(path) => path.to_path_buf(),
        None => match std::env::var_os(DISCOVERY_ENV) {
            Some(dir) => PathBuf::from(dir).join(DISCOVERY_FILE),
            None => discovery_dir(None).join(DISCOVERY_FILE),
        },
    };

    let text =
        std::fs::read_to_string(&path).map_err(|_| CliError::NoDaemon { path: path.clone() })?;
    let info: DiscoveryInfo =
        serde_json::from_str(&text).map_err(|error| CliError::BadDiscovery {
            path: path.clone(),
            detail: error.to_string(),
        })?;
    Ok((path, info))
}

/// A connected client.
#[derive(Debug)]
pub struct Client {
    stream: interprocess::local_socket::Stream,
    next_request_id: u64,
}

impl Client {
    /// Dial the daemon's control socket.
    pub fn connect(info: &DiscoveryInfo) -> Result<Self, CliError> {
        use interprocess::local_socket::traits::Stream as _;
        let name = crate::names::wrap_socket_name(&info.control_socket)?;
        let stream =
            interprocess::local_socket::Stream::connect(name).map_err(CliError::Unreachable)?;
        stream
            .set_nonblocking(false)
            .map_err(CliError::Unreachable)?;
        Ok(Self {
            stream,
            next_request_id: 1,
        })
    }

    /// Send one body and read its reply.
    ///
    /// Request ids increment so a reply can be matched to its request. The
    /// daemon echoes the id, and a mismatch means the stream has desynchronized
    /// — at which point every later reply on it would answer the wrong
    /// question, so the command fails rather than printing something plausible.
    pub fn call(&mut self, body: Body) -> Result<Body, CliError> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        let envelope = ProbeEnvelope {
            wire_version: 1,
            request_id,
            deadline_unix_ms: deadline_from_now(REPLY_TIMEOUT),
            body: Some(body),
        };
        write_frame(&mut self.stream, &envelope.encode_to_vec())?;

        let bytes = read_frame_with_cap(&mut self.stream, MAX_REPLY_BYTES)?;
        let reply = ProbeEnvelope::decode(bytes.as_slice())
            .map_err(|error| CliError::UnexpectedReply(error.to_string()))?;
        if reply.request_id != request_id {
            return Err(CliError::UnexpectedReply(format!(
                "reply id {} does not match request id {request_id}",
                reply.request_id
            )));
        }
        reply
            .body
            .ok_or_else(|| CliError::UnexpectedReply("reply had no body".into()))
    }
}

/// Absolute wall-clock deadline `timeout` from now.
///
/// Absolute rather than relative so a request queued behind other work cannot
/// silently extend its own budget.
fn deadline_from_now(timeout: Duration) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| (since + timeout).as_millis() as u64)
        .unwrap_or(0)
}

/// Fetch one URL from the daemon's HTTP surface with the bearer token.
///
/// A hand-rolled HTTP/1.1 GET rather than a client dependency: the CLI already
/// links `interprocess` and prost for the socket, and adding a TLS-capable
/// HTTP stack to talk to `127.0.0.1` would be the largest dependency in the
/// binary for the least of its work.
pub fn http_get(info: &DiscoveryInfo, path: &str) -> Result<Vec<u8>, CliError> {
    request(info, "GET", path, Duration::from_secs(30))
}

/// Send one request and return its body, failing on a non-200 status.
fn request(
    info: &DiscoveryInfo,
    method: &str,
    path: &str,
    timeout: Duration,
) -> Result<Vec<u8>, CliError> {
    use std::io::{BufRead, BufReader, Read, Write};

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", info.http_port))
        .map_err(CliError::Unreachable)?;
    // Bounded, so a wedged daemon fails the command instead of hanging a
    // terminal indefinitely.
    stream.set_read_timeout(Some(timeout))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Authorization: Bearer {}\r\nContent-Length: 0\r\n\
         Connection: close\r\n\r\n",
        info.bearer_token
    )?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);

    let mut chunked = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if line
            .to_ascii_lowercase()
            .starts_with("transfer-encoding: chunked")
        {
            chunked = true;
        }
    }

    let mut body = Vec::new();
    reader.read_to_end(&mut body)?;
    if chunked {
        body = dechunk(body);
    }

    if status != 200 {
        return Err(CliError::Refused(format!(
            "HTTP {status}: {}",
            String::from_utf8_lossy(&body).trim()
        )));
    }
    Ok(body)
}

/// POST one URL on the daemon's HTTP surface.
///
/// Shares `http_get`'s hand-rolled request for the same reason: the CLI
/// already links a socket transport, and adding a TLS-capable HTTP stack to
/// talk to `127.0.0.1` would be the largest dependency in the binary for the
/// least of its work.
///
/// The read timeout is generous because a profile request blocks for the whole
/// session — up to the daemon's sixty-second ceiling plus symbolization.
pub fn http_post(info: &DiscoveryInfo, path: &str) -> Result<Vec<u8>, CliError> {
    request(info, "POST", path, Duration::from_secs(180))
}

/// Undo `Transfer-Encoding: chunked`.
fn dechunk(raw: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = raw.as_slice();
    while let Some(split) = rest.windows(2).position(|w| w == b"\r\n") {
        let Ok(size) = std::str::from_utf8(&rest[..split])
            .ok()
            .map(str::trim)
            .map(|text| usize::from_str_radix(text, 16))
            .unwrap_or(Ok(0))
        else {
            break;
        };
        rest = &rest[split + 2..];
        if size == 0 || rest.len() < size {
            break;
        }
        out.extend_from_slice(&rest[..size]);
        let skip = size + 2.min(rest.len() - size);
        rest = &rest[skip..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn info() -> DiscoveryInfo {
        DiscoveryInfo {
            wire_version: 1,
            control_socket: "rpp-test".into(),
            http_port: 1,
            bearer_token: "t".repeat(64),
            daemon_pid: 1,
        }
    }

    #[test]
    fn a_missing_discovery_file_says_how_to_start_a_daemon() {
        let dir = TempDir::new().expect("temp dir");
        let error = load_discovery(Some(dir.path())).expect_err("must not find a daemon");
        let rendered = error.to_string();
        assert!(rendered.contains("no probe daemon found"));
        // Actionable, not just negative: the operator is told what to run.
        assert!(rendered.contains("rpprobed"));
    }

    #[test]
    fn a_corrupt_discovery_file_is_distinguished_from_an_absent_one() {
        // These call for different responses — start the daemon, versus the
        // daemon wrote something this build cannot read — so they must not
        // collapse into the same message.
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join(DISCOVERY_FILE), "not json").expect("write");
        match load_discovery(Some(dir.path())) {
            Err(CliError::BadDiscovery { .. }) => {}
            other => panic!("expected BadDiscovery, got {other:?}"),
        }
    }

    #[test]
    fn a_discovery_file_round_trips() {
        let dir = TempDir::new().expect("temp dir");
        let written = info();
        std::fs::write(
            dir.path().join(DISCOVERY_FILE),
            serde_json::to_string(&written).expect("serialize"),
        )
        .expect("write");
        let (_, read) = load_discovery(Some(dir.path())).expect("load");
        assert_eq!(read, written);
    }

    #[test]
    fn an_explicit_file_path_is_used_directly() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("elsewhere.json");
        std::fs::write(&path, serde_json::to_string(&info()).expect("serialize")).expect("write");
        let (used, _) = load_discovery(Some(&path)).expect("load");
        assert_eq!(used, path);
    }

    #[test]
    fn dechunking_reassembles_a_split_body() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n".to_vec();
        assert_eq!(dechunk(raw), b"Wikipedia".to_vec());
    }

    #[test]
    fn a_deadline_is_absolute_and_in_the_future() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64;
        let deadline = deadline_from_now(Duration::from_secs(30));
        assert!(deadline >= now + 29_000 && deadline <= now + 31_000);
    }
}
