//! Publishing the broker's HTTP port so callers can find it (#483).
//!
//! # Why a file
//!
//! Two of the three [`BrokerHttpPort`] modes end with a port the operator did
//! not choose: `Dynamic` always, and `StaticOrFallback` whenever the preferred
//! port was taken. In those cases the only record of the real port is the log,
//! which a CLI cannot reasonably scrape and an operator should not have to.
//!
//! Writing it to a predictable path makes the answer available to anything
//! that can read the runtime directory, using the same convention the broker
//! already uses for its pipe path.
//!
//! # Why the file is rewritten, not appended
//!
//! A stale port is worse than none: a reader that connects to it reaches
//! whatever now owns that port, or nothing, and cannot tell which. The file
//! therefore always describes the current bind, and is removed when the
//! broker releases it.
//!
//! [`BrokerHttpPort`]: super::broker_http_port::BrokerHttpPort

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use super::secure_dir::ensure_private_dir;

/// Extension for the published-port file.
const EXTENSION: &str = "http_port";

/// Path of the file publishing `program`'s broker HTTP port.
///
/// The program name is part of the filename because one machine may run a
/// broker per program type, and two brokers writing the same path would each
/// see the other's port.
pub fn http_port_path(runtime_dir: &Path, program: &str) -> PathBuf {
    runtime_dir.join(format!("broker-v2-{program}.{EXTENSION}"))
}

/// Publish `addr` and `port` for `program`.
///
/// The directory is created owner-only if absent, matching how the broker
/// treats the rest of its runtime state — the file names a reachable local
/// endpoint, which is not something to leave world-readable on a shared host.
pub fn publish_http_port(
    runtime_dir: &Path,
    program: &str,
    addr: IpAddr,
    port: u16,
) -> io::Result<PathBuf> {
    ensure_private_dir(runtime_dir)?;
    let path = http_port_path(runtime_dir, program);
    // `addr:port` rather than bare port: under a BIND override the broker may
    // not be on loopback, and a reader that assumed 127.0.0.1 would fail in
    // exactly the container deployment the override exists for.
    std::fs::write(&path, format!("{addr}:{port}\n"))?;
    Ok(path)
}

/// Read the published endpoint for `program`.
///
/// Returns `Ok(None)` when no broker has published one — an absent file means
/// "not running", which is a normal state rather than an error.
pub fn read_http_port(runtime_dir: &Path, program: &str) -> io::Result<Option<(IpAddr, u16)>> {
    let path = http_port_path(runtime_dir, program);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    parse_endpoint(text.trim())
        .map(Some)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("malformed {path:?}")))
}

/// Split `addr:port`, tolerating the bracketed IPv6 form.
///
/// Splitting on the last colon rather than the first is what makes IPv6 work:
/// `::1:8080` has colons throughout, and only the final one separates the
/// port.
fn parse_endpoint(text: &str) -> Option<(IpAddr, u16)> {
    let (addr, port) = text.rsplit_once(':')?;
    let addr = addr.trim_start_matches('[').trim_end_matches(']');
    Some((addr.parse().ok()?, port.parse().ok()?))
}

/// Remove `program`'s published port.
///
/// Best-effort and idempotent: a file that is already gone is success, since
/// the goal is that no stale port remains.
pub fn unpublish_http_port(runtime_dir: &Path, program: &str) -> io::Result<()> {
    match std::fs::remove_file(http_port_path(runtime_dir, program)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn a_published_port_reads_back() {
        let d = dir();
        publish_http_port(d.path(), "zccache", Ipv4Addr::LOCALHOST.into(), 8080).expect("publish");
        assert_eq!(
            read_http_port(d.path(), "zccache").expect("read"),
            Some((Ipv4Addr::LOCALHOST.into(), 8080))
        );
    }

    /// An absent file is "no broker running", not a failure.
    #[test]
    fn an_absent_file_is_not_an_error() {
        let d = dir();
        assert_eq!(read_http_port(d.path(), "nobody").expect("read"), None);
    }

    /// Two programs must not read each other's port.
    #[test]
    fn programs_do_not_share_a_file() {
        let d = dir();
        publish_http_port(d.path(), "alpha", Ipv4Addr::LOCALHOST.into(), 1111).expect("a");
        publish_http_port(d.path(), "bravo", Ipv4Addr::LOCALHOST.into(), 2222).expect("b");

        assert_eq!(read_http_port(d.path(), "alpha").unwrap().unwrap().1, 1111);
        assert_eq!(read_http_port(d.path(), "bravo").unwrap().unwrap().1, 2222);
    }

    /// Rebinding must replace the port, never leave the old one readable.
    #[test]
    fn republishing_replaces_the_previous_port() {
        let d = dir();
        publish_http_port(d.path(), "p", Ipv4Addr::LOCALHOST.into(), 1000).expect("first");
        publish_http_port(d.path(), "p", Ipv4Addr::LOCALHOST.into(), 2000).expect("second");
        assert_eq!(read_http_port(d.path(), "p").unwrap().unwrap().1, 2000);
    }

    #[test]
    fn unpublishing_removes_the_file_and_is_idempotent() {
        let d = dir();
        publish_http_port(d.path(), "p", Ipv4Addr::LOCALHOST.into(), 1234).expect("publish");
        unpublish_http_port(d.path(), "p").expect("first remove");
        assert_eq!(read_http_port(d.path(), "p").expect("read"), None);
        unpublish_http_port(d.path(), "p").expect("removing twice is not an error");
    }

    /// A non-loopback bind must survive the round trip — that is the whole
    /// point of recording the address rather than assuming 127.0.0.1.
    #[test]
    fn a_non_loopback_address_round_trips() {
        let d = dir();
        let addr: IpAddr = "0.0.0.0".parse().unwrap();
        publish_http_port(d.path(), "p", addr, 9000).expect("publish");
        assert_eq!(read_http_port(d.path(), "p").unwrap(), Some((addr, 9000)));
    }

    /// IPv6 has colons throughout, so the port must be split from the last
    /// one, not the first.
    #[test]
    fn an_ipv6_address_round_trips() {
        let d = dir();
        let addr: IpAddr = "::1".parse().unwrap();
        publish_http_port(d.path(), "p", addr, 7000).expect("publish");
        assert_eq!(read_http_port(d.path(), "p").unwrap(), Some((addr, 7000)));
    }

    /// Garbage must be reported, not silently treated as "not running" — the
    /// two call for different responses.
    #[test]
    fn a_malformed_file_is_an_error_not_an_absence() {
        let d = dir();
        std::fs::write(http_port_path(d.path(), "p"), "not-an-endpoint").expect("write");
        let err = read_http_port(d.path(), "p").expect_err("garbage is not an absence");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn the_directory_is_created_when_absent() {
        let d = dir();
        let nested = d.path().join("does").join("not").join("exist");
        publish_http_port(&nested, "p", Ipv4Addr::LOCALHOST.into(), 4321).expect("publish");
        assert_eq!(read_http_port(&nested, "p").unwrap().unwrap().1, 4321);
    }
}
