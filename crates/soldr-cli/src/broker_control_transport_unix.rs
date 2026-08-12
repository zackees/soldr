use interprocess::local_socket::traits::Stream as _;
use std::io;
use std::time::Duration;

pub(super) fn connect(
    endpoint: String,
    route_timeout: Duration,
    request_timeout: Duration,
    service_name: String,
) -> io::Result<crate::daemon::client::BoxedControlStream> {
    let name = crate::session_transport::local_session_name(&endpoint)?;
    let stream = interprocess::local_socket::ConnectOptions::new()
        .name(name)
        .wait_mode(interprocess::ConnectWaitMode::Timeout(route_timeout))
        .connect_sync()?;
    stream.set_recv_timeout(Some(route_timeout))?;
    stream.set_send_timeout(Some(route_timeout))?;
    let stream = super::negotiate_control_tunnel(stream, route_timeout, service_name)?;
    stream.set_recv_timeout(Some(request_timeout.max(Duration::from_millis(200))))?;
    stream.set_send_timeout(Some(request_timeout))?;
    Ok(Box::new(stream))
}
