use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message;
use running_process::broker::backend_handle::BackendHandle;
use running_process::broker::manifest;
use running_process::broker::protocol::{write_frame, CacheManifest, Hello, Operation};

fn main() -> Result<(), Box<dyn Error>> {
    let now = unix_now_ms();

    let hello = Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "minimal-consumer".to_string(),
        wanted_version: "1.0.0".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        request_id: format!("minimal-consumer-{now}"),
        peer_pid: std::process::id(),
        client_lib_name: "running-process-minimal-consumer".to_string(),
        client_lib_version: env!("CARGO_PKG_VERSION").to_string(),
        client_keepalive_secs: 30,
        ..Default::default()
    };

    let mut hello_body = Vec::new();
    hello.encode(&mut hello_body)?;

    let mut framed = Vec::new();
    let bytes_written = write_frame(&mut framed, &hello_body)?;

    let manifest = CacheManifest {
        host: Some(running_process::broker::host_identity::current()),
        current_operation: Some(Operation {
            kind: 0,
            started_at_unix_ms: now,
            expected_done_unix_ms: 0,
        }),
        valid_until_unix_ms: now + 30_000,
        service_name: hello.service_name.clone(),
        service_version: hello.wanted_version.clone(),
        broker_envelope_version: "v1".to_string(),
        created_at_unix_ms: now,
        last_active_unix_ms: now,
        broker_instance: "shared".to_string(),
        provides: vec!["hello-world".to_string()],
        bundle_id: "minimal-consumer-demo".to_string(),
        ..Default::default()
    };
    let manifest = manifest::manifest_with_self_sha256(&manifest)?;
    assert_eq!(manifest.self_sha256.len(), 32);

    // No daemon identity was recorded, so this foundation probe returns None.
    assert!(BackendHandle::probe_manifest(&manifest).is_none());

    println!(
        "encoded {} bytes for {} {} and prepared a {} byte manifest digest",
        bytes_written,
        manifest.service_name,
        manifest.service_version,
        manifest.self_sha256.len()
    );

    Ok(())
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
