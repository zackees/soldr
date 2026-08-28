//! The hot-path receipt ack stays best-effort (soldr#2785).
//!
//! soldr#2558 added an ack to `submit_fire_and_forget` so a connection closed
//! before the daemon's accept could not discard the buffered frame, and made
//! the wait **bounded rather than required** precisely so an older daemon that
//! never acks keeps working. soldr#2785 stopped discarding the ack's outcome so
//! a missing one is at least visible.
//!
//! Those two must not collide. Observing a missing ack must not quietly promote
//! it to a failure: this path's whole contract is that the caller does not learn
//! the outcome, and failing here would turn every pre-#2558 daemon into a hard
//! error on the wrapper's per-invocation hot path.
//!
//! Driven through `install_control_connector` rather than a real socket. That
//! seam is process-local and cross-platform, which matters twice over: the
//! Windows leg of this code takes a named-pipe branch that a `UnixListener`
//! stub cannot reach, and a host `#[cfg]` outside `crates/soldr-platform` is a
//! boundary-ratchet violation (`.github/scripts/platform_cfg_boundary_ratchet.py`).
//!
//! `CONTROL_CONNECTOR` is a `OnceLock`, so the stub connector can be installed
//! exactly once per process and the first install wins for every later caller
//! in that process. This test therefore needs a process in which nothing else
//! has claimed that slot — and, just as importantly, in which its stub is not
//! left standing for a sibling test that expects to dial a real daemon, which
//! is what `cli_daemon_target_touch.rs` does.
//!
//! That used to be arranged by making this file its own integration target.
//! It no longer is: soldr#2934 consolidated `crates/soldr-cli/tests/` so each
//! category directory builds as a single test binary (98 separate static links
//! produced a 3.3 GB CI archive that exhausted a runner's disk), and this file
//! is now a module in the `daemon` binary next to `cli_daemon_target_touch.rs`.
//! The isolation now comes from cargo-nextest running every test in its own
//! process, which is a stronger guarantee than the per-file binary gave — that
//! binary still shared one process across all the tests inside it.
//!
//! The caveat: under plain `cargo test`, which shares a single process across
//! every test in the `daemon` binary, this test and its siblings can race on
//! that `OnceLock` — whichever runs first installs the connector the others are
//! then stuck with. That is one more reason the suite must be run with
//! `soldr cargo nextest run`.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use soldr_cli::daemon::client::{
    self, install_control_connector, BoxedControlStream, ControlConnector,
};
use soldr_cli::daemon::protocol::Request;

/// A peer that takes the frame and answers nothing.
///
/// Reads report EOF, which is what a daemon that accepted the connection and
/// then went away looks like from here -- and is the shape the msvc lane keeps
/// producing: no drop logged, no upsert failure, no ack.
struct SilentPeer {
    written: Arc<AtomicUsize>,
}

struct SilentStream {
    written: Arc<AtomicUsize>,
}

impl Read for SilentStream {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

impl Write for SilentStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written.fetch_add(buf.len(), Ordering::SeqCst);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ControlConnector for SilentPeer {
    fn connect(
        &self,
        _endpoint_marker: &Path,
        _timeout: Duration,
    ) -> std::io::Result<BoxedControlStream> {
        Ok(Box::new(SilentStream {
            written: Arc::clone(&self.written),
        }))
    }
}

#[test]
fn a_peer_that_never_acks_is_still_a_successful_submit() {
    let written = Arc::new(AtomicUsize::new(0));
    install_control_connector(Arc::new(SilentPeer {
        written: Arc::clone(&written),
    }))
    .expect("no other connector may be installed in this test binary");

    let result = client::submit_fire_and_forget(
        Path::new("endpoint-marker-unused-by-the-override"),
        &Request::RecordTargetTouch {
            path: "/some/workspace/target".to_string(),
            unix_seconds: 1_700_000_000,
        },
    );

    assert!(
        result.is_ok(),
        "a missing ack must stay best-effort, not become an error: {result:?}"
    );
    assert!(
        written.load(Ordering::SeqCst) > 0,
        "the request frame should still have been written to the peer"
    );
}
