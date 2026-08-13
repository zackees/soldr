async fn write_progress(
    stream: &mut interprocess::local_socket::tokio::Stream,
    request: &Frame,
    progress: RouteProgress,
) -> io::Result<()> {
    write_frame_async(
        stream,
        &Frame {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Event as i32,
            payload_protocol: ROUTE_PROGRESS_PAYLOAD_PROTOCOL,
            payload: progress.encode_to_vec(),
            request_id: request.request_id,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: request.traceparent.clone(),
            tracestate: request.tracestate.clone(),
        },
    )
    .await
}

async fn write_hello_reply(
    stream: &mut interprocess::local_socket::tokio::Stream,
    request: &Frame,
    reply: &HelloReply,
) -> io::Result<()> {
    write_frame_async(
        stream,
        &Frame {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Response as i32,
            payload_protocol: CONTROL_PAYLOAD_PROTOCOL,
            payload: reply.encode_to_vec(),
            request_id: request.request_id,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: request.traceparent.clone(),
            tracestate: request.tracestate.clone(),
        },
    )
    .await
}

async fn read_frame_async(
    stream: &mut interprocess::local_socket::tokio::Stream,
) -> io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut header = [0_u8; 5];
    stream.read_exact(&mut header).await?;
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
    stream.read_exact(&mut body).await?;
    Ok(body)
}

async fn write_frame_async(
    stream: &mut interprocess::local_socket::tokio::Stream,
    frame: &Frame,
) -> io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let body = frame.encode_to_vec();
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "broker frame exceeds u32"))?;
    let mut header = [0_u8; 5];
    header[0] = ENVELOPE_VERSION;
    header[1..].copy_from_slice(&len.to_le_bytes());
    stream.write_all(&header).await?;
    stream.write_all(&body).await?;
    stream.flush().await
}

fn bind_listener(
    endpoint: &crate::broker_identity::ResolvedBrokerEndpoint,
) -> io::Result<interprocess::local_socket::tokio::Listener> {
    create_listener(&endpoint.bind_endpoint)
}

fn create_listener(endpoint: &str) -> io::Result<interprocess::local_socket::tokio::Listener> {
    crate::platform::ipc::broker::bind_listener(endpoint, BROKER_LISTEN_BACKLOG)
}
