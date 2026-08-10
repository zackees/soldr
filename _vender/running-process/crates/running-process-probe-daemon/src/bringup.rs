//! Bring-up: decide whether this process becomes the daemon or joins one.
//!
//! # Bind-wins, never check-then-act
//!
//! The election is the `TcpListener::bind` return value and nothing else.
//! The tempting shape —
//!
//! ```text
//! if !port_is_taken(p) { bind(p) }   // WRONG
//! ```
//!
//! — is a TOCTOU: between the probe and the bind, another process can bind the
//! same port, and both then believe they won. Because `bind` is atomic in the
//! kernel, exactly one caller can succeed, so its result is the only signal
//! that can't lie. Everything here is built on that.
//!
//! # A reachable port is not an identity
//!
//! Any process can listen on the beacon port. Before treating a listener as
//! the daemon, a client completes a framed nonce handshake; a peer that fails
//! it is classified [`Role::StrangerOnBeacon`] and is never trusted. Without
//! that step a decoy could impersonate the daemon and harvest registrations.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::time::Duration;

use prost::Message as _;
use running_process::broker::protocol::framing::{read_frame, write_frame};
use running_process_probe::probe_diag::v1::{
    probe_envelope::Body, ProbeEnvelope, ProbeHello, ProbeHelloReply,
};

use crate::discovery::DiscoveryInfo;

/// Fixed magic a genuine daemon returns in its handshake reply. A listener
/// that speaks the framing but isn't rpprobed still fails on this.
pub const PROBE_HELLO_MAGIC: u64 = 0x7270_726f_6265_0001;

/// Base of the per-user beacon range.
const BEACON_BASE: u16 = 47_000;
/// Deterministic per-user spread so two users don't contend for one port.
const BEACON_SPREAD: u16 = 1_000;
/// Candidate ports probed before electing.
const BEACON_CANDIDATES: u16 = 10;

/// Bounded so a wedged or malicious peer can't stall bring-up indefinitely.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(500);

/// What this process turned out to be.
#[derive(Debug)]
pub enum Role {
    /// We won the election and own the beacon listener.
    Daemon {
        /// The bound beacon listener; holding it keeps the election won.
        beacon: TcpListener,
        /// Port the beacon is bound to.
        port: u16,
    },
    /// A genuine daemon answered; here is how to reach it.
    Client(Box<DiscoveryInfo>, u16),
    /// Something is on the beacon port but failed the identity handshake.
    /// Deliberately distinct from `Client` — we must not register with it.
    StrangerOnBeacon(u16),
}

/// Bring-up configuration.
#[derive(Debug, Clone)]
pub struct BringUpConfig {
    /// Explicit override (`--beacon-port` / env). Collapses the candidate
    /// range to this single port, but still goes through bind-wins.
    pub beacon_port: Option<u16>,
    /// Per-user seed, normally the SID hash.
    pub sid_hash: String,
}

impl BringUpConfig {
    /// Deterministic per-user starting port.
    fn seed_port(&self) -> u16 {
        let mut acc: u16 = 0;
        for b in self.sid_hash.as_bytes().iter().take(8) {
            acc = acc.wrapping_mul(31).wrapping_add(u16::from(*b));
        }
        BEACON_BASE + (acc % BEACON_SPREAD)
    }

    /// Ports to probe, in order.
    pub fn beacon_ports(&self) -> Vec<u16> {
        match self.beacon_port {
            Some(p) => vec![p],
            None => {
                let start = self.seed_port();
                (0..BEACON_CANDIDATES)
                    .map(|i| start.saturating_add(i))
                    .collect()
            }
        }
    }

    /// The port we bind if no daemon answers.
    pub fn elect_port(&self) -> u16 {
        self.beacon_port.unwrap_or_else(|| self.seed_port())
    }
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

/// Perform the client side of the identity handshake over `stream`.
///
/// Returns the daemon's `DiscoveryInfo` only if the reply echoes our nonce and
/// carries the expected magic. Any deviation is an error, which the caller
/// turns into `StrangerOnBeacon`.
pub fn identity_handshake(stream: &mut TcpStream) -> io::Result<DiscoveryInfo> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| io::Error::other(format!("getrandom: {e}")))?;

    let hello = ProbeEnvelope {
        wire_version: 1,
        request_id: 1,
        deadline_unix_ms: 0,
        body: Some(Body::ProbeHello(ProbeHello {
            nonce: nonce.to_vec(),
        })),
    };
    write_frame(stream, &hello.encode_to_vec()).map_err(|e| io::Error::other(e.to_string()))?;

    let bytes = read_frame(stream).map_err(|e| io::Error::other(e.to_string()))?;
    let env = ProbeEnvelope::decode(bytes.as_slice())
        .map_err(|e| io::Error::other(format!("decode reply: {e}")))?;

    let Some(Body::ProbeHelloReply(reply)) = env.body else {
        return Err(io::Error::other("peer did not send ProbeHelloReply"));
    };
    if reply.magic != PROBE_HELLO_MAGIC {
        return Err(io::Error::other("peer returned wrong protocol magic"));
    }
    // Echo check defeats a canned reply captured from an earlier exchange.
    if reply.nonce_echo != nonce {
        return Err(io::Error::other("peer did not echo our nonce"));
    }

    Ok(DiscoveryInfo {
        wire_version: 1,
        control_socket: reply.control_socket,
        http_port: u16::try_from(reply.http_port).unwrap_or(0),
        // Never travels over the beacon — the reader loads it from the
        // owner-only discovery file.
        bearer_token: String::new(),
        daemon_pid: u32::try_from(reply.daemon_pid).unwrap_or(0),
    })
}

/// Serve one identity handshake as the daemon.
pub fn answer_identity_handshake(stream: &mut TcpStream, info: &DiscoveryInfo) -> io::Result<()> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let bytes = read_frame(stream).map_err(|e| io::Error::other(e.to_string()))?;
    let env = ProbeEnvelope::decode(bytes.as_slice())
        .map_err(|e| io::Error::other(format!("decode hello: {e}")))?;
    let Some(Body::ProbeHello(hello)) = env.body else {
        return Err(io::Error::other("peer did not send ProbeHello"));
    };

    let reply = ProbeEnvelope {
        wire_version: 1,
        request_id: env.request_id,
        deadline_unix_ms: 0,
        body: Some(Body::ProbeHelloReply(ProbeHelloReply {
            nonce_echo: hello.nonce,
            magic: PROBE_HELLO_MAGIC,
            daemon_pid: u64::from(std::process::id()),
            control_socket: info.control_socket.clone(),
            http_port: u32::from(info.http_port),
        })),
    };
    write_frame(stream, &reply.encode_to_vec()).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(())
}

/// Elect: become the daemon, or find the one that already exists.
///
/// See the module docs for why this is bind-wins rather than check-then-act.
pub fn bring_up(cfg: &BringUpConfig) -> io::Result<Role> {
    // 1. Is a daemon already answering?
    for port in cfg.beacon_ports() {
        let Ok(mut stream) = TcpStream::connect_timeout(&loopback(port), CONNECT_TIMEOUT) else {
            continue;
        };
        return match identity_handshake(&mut stream) {
            Ok(info) => Ok(Role::Client(Box::new(info), port)),
            // Reachable but not us. Never fall through to binding — something
            // owns the port, and never trust it either.
            Err(_) => Ok(Role::StrangerOnBeacon(port)),
        };
    }

    // 2. Nobody answered. Elect by binding; the bind IS the election.
    let port = cfg.elect_port();
    match TcpListener::bind(loopback(port)) {
        Ok(beacon) => Ok(Role::Daemon { beacon, port }),
        // Someone won between step 1 and here. That is the race this design
        // expects, not an error — join them.
        Err(e) if crate::names::is_already_bound_error(&e) => {
            let mut stream = TcpStream::connect_timeout(&loopback(port), HANDSHAKE_TIMEOUT)?;
            match identity_handshake(&mut stream) {
                Ok(info) => Ok(Role::Client(Box::new(info), port)),
                Err(_) => Ok(Role::StrangerOnBeacon(port)),
            }
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(port: Option<u16>) -> BringUpConfig {
        BringUpConfig {
            beacon_port: port,
            sid_hash: "0123456789abcdef".into(),
        }
    }

    #[test]
    fn seed_port_is_deterministic_and_in_range() {
        let a = cfg(None).seed_port();
        let b = cfg(None).seed_port();
        assert_eq!(a, b);
        assert!((BEACON_BASE..BEACON_BASE + BEACON_SPREAD).contains(&a));
    }

    #[test]
    fn different_users_get_different_seed_ports() {
        let mut other = cfg(None);
        other.sid_hash = "fedcba9876543210".into();
        assert_ne!(cfg(None).seed_port(), other.seed_port());
    }

    #[test]
    fn explicit_port_collapses_the_candidate_range() {
        assert_eq!(cfg(Some(45_999)).beacon_ports(), vec![45_999]);
        assert_eq!(cfg(Some(45_999)).elect_port(), 45_999);
    }

    #[test]
    fn seeded_range_offers_multiple_candidates() {
        assert_eq!(cfg(None).beacon_ports().len(), BEACON_CANDIDATES as usize);
    }

    /// Two binds of one port: exactly one succeeds, and the loser's error is
    /// classified as already-bound rather than a hard failure.
    #[test]
    fn second_bind_of_same_port_is_classified_already_bound() {
        let first = TcpListener::bind(loopback(0)).expect("first bind");
        let port = first.local_addr().unwrap().port();
        let err = TcpListener::bind(loopback(port)).expect_err("second bind must fail");
        assert!(
            crate::names::is_already_bound_error(&err),
            "unexpected kind {:?}",
            err.kind()
        );
    }

    /// A listener that accepts but doesn't speak the protocol must be rejected,
    /// not mistaken for the daemon.
    #[test]
    fn decoy_listener_fails_the_identity_handshake() {
        let decoy = TcpListener::bind(loopback(0)).unwrap();
        let port = decoy.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = decoy.accept() {
                use std::io::Write as _;
                let _ = s.write_all(b"garbage, definitely not a frame");
            }
        });

        let mut stream = TcpStream::connect_timeout(&loopback(port), CONNECT_TIMEOUT).unwrap();
        assert!(
            identity_handshake(&mut stream).is_err(),
            "decoy must not pass the handshake"
        );
    }

    /// End-to-end handshake against a real responder.
    #[test]
    fn genuine_handshake_returns_daemon_endpoints() {
        let info = DiscoveryInfo {
            wire_version: 1,
            control_socket: "/tmp/probe.sock".into(),
            http_port: 51_515,
            bearer_token: "secret".into(),
            daemon_pid: std::process::id(),
        };
        let listener = TcpListener::bind(loopback(0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = info.clone();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let _ = answer_identity_handshake(&mut s, &served);
            }
        });

        let mut stream = TcpStream::connect_timeout(&loopback(port), CONNECT_TIMEOUT).unwrap();
        let got = identity_handshake(&mut stream).expect("handshake must succeed");
        assert_eq!(got.control_socket, info.control_socket);
        assert_eq!(got.http_port, info.http_port);
        // The secret must not ride the beacon.
        assert!(got.bearer_token.is_empty());
    }

    /// bring_up against a decoy classifies it as a stranger and does not bind.
    #[test]
    fn bring_up_reports_stranger_for_a_non_protocol_listener() {
        let decoy = TcpListener::bind(loopback(0)).unwrap();
        let port = decoy.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = decoy.accept() {
                use std::io::Write as _;
                let _ = s.write_all(b"nope");
            }
        });

        match bring_up(&cfg(Some(port))).expect("bring_up") {
            Role::StrangerOnBeacon(p) => assert_eq!(p, port),
            other => panic!("expected StrangerOnBeacon, got {other:?}"),
        }
    }

    /// With a free port and no daemon, we become the daemon.
    #[test]
    fn bring_up_elects_daemon_on_a_free_port() {
        let probe = TcpListener::bind(loopback(0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe); // release so the election can claim it

        match bring_up(&cfg(Some(port))).expect("bring_up") {
            Role::Daemon { port: p, .. } => assert_eq!(p, port),
            other => panic!("expected Daemon, got {other:?}"),
        }
    }
}
