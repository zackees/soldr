//! User-facing daemon control traffic over the one stable broker listener.
//!
//! `soldr-daemon` retains a private control listener for its own lifecycle
//! implementation. The CLI installs this connector once at process startup,
//! so status, shutdown, cache metadata, build logs, GC, and session accounting
//! never derive or dial that daemon endpoint themselves.

use interprocess::local_socket::traits::Stream as _;
use prost::Message as _;
use running_process::broker::protocol::{
    encode_framed, Frame, FrameKind, PayloadEncoding, ENVELOPE_VERSION, MAX_FRAME_BYTES,
    PROTOCOL_VERSION,
};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::broker_server::{
    DaemonControlTunnelReply, DaemonControlTunnelRequest, DAEMON_CONTROL_PAYLOAD_PROTOCOL,
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn install() -> Result<(), &'static str> {
    crate::daemon::client::install_control_connector(Arc::new(BrokerControlConnector))
}

struct BrokerControlConnector;

impl crate::daemon::client::ControlConnector for BrokerControlConnector {
    fn connect(
        &self,
        _endpoint_marker: &Path,
        timeout: Duration,
    ) -> io::Result<crate::daemon::client::BoxedControlStream> {
        // Route derivation may hash a freshly-built daemon image. Complete it
        // before connecting so the broker's post-connect first-frame deadline
        // measures transport responsiveness, not client-side preparation.
        let service_name = crate::daemon::backend_handle_adoption::broker_service_name()?;
        let endpoint =
            crate::broker_identity::ResolvedBrokerEndpoint::resolve().map_err(io::Error::other)?;
        let name = crate::session_transport::local_session_name(&endpoint.bind_endpoint)?;
        // A replacement broker performs one exact BackendHandle verification
        // before it can re-adopt a surviving daemon. That verification hashes
        // the executable and can exceed the ordinary 2s status reply budget
        // for a large debug image. The route handshake gets a separate bounded
        // allowance; once accepted, restore the caller's request timeout.
        let route_timeout = route_handshake_timeout(timeout);
        let stream = interprocess::local_socket::ConnectOptions::new()
            .name(name)
            .wait_mode(interprocess::ConnectWaitMode::Timeout(route_timeout))
            .connect_sync()?;
        stream.set_recv_timeout(Some(route_timeout))?;
        stream.set_send_timeout(Some(route_timeout))?;
        let stream = negotiate_control_tunnel(stream, route_timeout, service_name)?;
        stream.set_recv_timeout(Some(timeout.max(Duration::from_millis(200))))?;
        stream.set_send_timeout(Some(timeout))?;
        Ok(Box::new(stream))
    }
}

fn route_handshake_timeout(request_timeout: Duration) -> Duration {
    if request_timeout <= Duration::from_millis(100) {
        request_timeout
    } else {
        request_timeout.max(Duration::from_secs(30))
    }
}

fn negotiate_control_tunnel(
    mut stream: interprocess::local_socket::Stream,
    timeout: Duration,
    service_name: String,
) -> io::Result<interprocess::local_socket::Stream> {
    let host = running_process::broker::host_identity::current();
    let request = DaemonControlTunnelRequest {
        service_name,
        machine_id: host.machine_id,
        boot_id: host.boot_id,
    };
    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed).max(1);
    let frame = Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: DAEMON_CONTROL_PAYLOAD_PROTOCOL,
        payload: request.encode_to_vec(),
        request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: unix_deadline_ms(timeout),
        traceparent: String::new(),
        tracestate: String::new(),
    };
    stream.write_all(&encode_framed(&frame).map_err(io::Error::other)?)?;
    stream.flush()?;

    let reply_frame = read_broker_frame(&mut stream)?;
    if reply_frame.request_id != request_id
        || reply_frame.payload_protocol != DAEMON_CONTROL_PAYLOAD_PROTOCOL
        || FrameKind::try_from(reply_frame.kind) != Ok(FrameKind::Response)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker returned a mismatched daemon-control tunnel reply",
        ));
    }
    let reply = DaemonControlTunnelReply::decode(reply_frame.payload.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !reply.accepted {
        return Err(io::Error::new(
            if reply.not_running {
                io::ErrorKind::NotFound
            } else {
                io::ErrorKind::PermissionDenied
            },
            reply.error_detail,
        ));
    }
    Ok(stream)
}

fn read_broker_frame(stream: &mut impl Read) -> io::Result<Frame> {
    let mut header = [0_u8; 5];
    stream.read_exact(&mut header)?;
    if header[0] != ENVELOPE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported broker framing version",
        ));
    }
    let len = u32::from_le_bytes(header[1..].try_into().expect("four bytes")) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker frame exceeds the maximum size",
        ));
    }
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body)?;
    Frame::decode(body.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn unix_deadline_ms(timeout: Duration) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_add(timeout)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(control_tunnel_deadline_is_future_and_bounded, {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let deadline = unix_deadline_ms(Duration::from_secs(2));
        assert!(deadline >= before + 1_900);
        assert!(deadline <= before + 2_100);
    });

    crate::timed_test!(hot_path_timeout_is_not_inflated_by_route_handshake, {
        assert_eq!(
            route_handshake_timeout(Duration::from_millis(40)),
            Duration::from_millis(40)
        );
        assert_eq!(
            route_handshake_timeout(Duration::from_secs(2)),
            Duration::from_secs(30)
        );
        assert_eq!(
            route_handshake_timeout(Duration::from_secs(45)),
            Duration::from_secs(45)
        );
    });
}
