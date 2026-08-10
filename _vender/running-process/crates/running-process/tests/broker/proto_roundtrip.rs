//! Phase 0 of #228 — encode/decode round-trip for every message and
//! enum the v1 broker schemas declare. A regression here means the
//! frozen-forever wire contract is broken.

#![cfg(feature = "client")]

use std::collections::HashMap;

use prost::Message;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, AdminReply, AdminReplyKind, AdminRequest, AdminVerb,
    BrokerIsolation, CacheManifest, CacheRoot, CacheRootKind, CleanupPolicy, DaemonProcess,
    Endpoint, ErrorCode, EventKind, Frame, FrameKind, HandoffAck, HandoffOffer, Hello, HelloReply,
    HostIdentity, LifecycleEvent, ManifestRef, Negotiated, ObservabilityInfo, Operation,
    OperationKind, Ownership, PayloadEncoding, Quota, Refused, ServiceDefinition,
    StorageDisposition, TeardownHook, TeardownKind,
};

fn assert_roundtrip<M: Message + PartialEq + std::fmt::Debug + Default>(msg: M) {
    let mut buf = Vec::new();
    msg.encode(&mut buf).expect("encode");
    let back = M::decode(buf.as_slice()).expect("decode");
    assert_eq!(
        msg,
        back,
        "roundtrip mismatch for {}",
        std::any::type_name::<M>()
    );
}

#[test]
fn frame_roundtrip() {
    let frame = Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: b"hello".to_vec(),
        request_id: 7,
        payload_encoding: PayloadEncoding::Zstd as i32,
        deadline_unix_ms: 1_700_000_000_000,
        traceparent: "00-trace-span-01".into(),
        tracestate: "vendor=value".into(),
    };
    assert_roundtrip(frame);
}

#[test]
fn hello_roundtrip() {
    let hello = Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/0.5.0".into(),
        client_capabilities: 0xDEAD_BEEF,
        auth_token: b"reserved-token".to_vec(),
        request_id: "req-42".into(),
        connection_id: 0,
        peer_pid: 4242,
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: vec![0xAA; 32],
        capability_token: vec![],
        client_keepalive_secs: 60,
    };
    assert_roundtrip(hello);
}

#[test]
fn hello_reply_negotiated_roundtrip() {
    let neg = Negotiated {
        negotiated_protocol: 1,
        daemon_version: "running-process-broker-v1/4.0.3".into(),
        backend_pipe: r"\\.\pipe\rpb-v1-abc-zccache-1.11.20-deadbeef".into(),
        warnings: vec!["client version older than recommended".into()],
        server_capabilities: 0x01,
        keepalive_interval_secs: 60,
        handle_passed_token: vec![],
        connection_id: 99,
    };
    let reply = HelloReply {
        result: Some(HelloReplyResult::Negotiated(neg)),
    };
    assert_roundtrip(reply);
}

#[test]
fn hello_reply_refused_roundtrip() {
    let mut details = HashMap::new();
    details.insert("hint".into(), "upgrade to >=1.12".into());
    let refused = Refused {
        reason: "version unsupported".into(),
        daemon_min_protocol: 1,
        daemon_max_protocol: 1,
        code: ErrorCode::ErrorVersionUnsupported as i32,
        details,
        retry_after_ms: 500,
    };
    let reply = HelloReply {
        result: Some(HelloReplyResult::Refused(refused)),
    };
    assert_roundtrip(reply);
}

#[test]
fn handoff_offer_roundtrip() {
    let offer = HandoffOffer {
        handle_value: 0xB0B,
        token: vec![0x42; 16],
        service_name: "zccache".into(),
        correlation_id: 0xC0FFEE,
    };
    assert_roundtrip(offer);
}

#[test]
fn handoff_ack_roundtrip() {
    let ack = HandoffAck {
        token: vec![0x42; 16],
        accepted: false,
        error_detail: "handoff token mismatch".into(),
        correlation_id: 0xC0FFEE,
    };
    assert_roundtrip(ack);
}

#[test]
fn admin_request_roundtrip() {
    let request = AdminRequest {
        verb: AdminVerb::BackendHealth as i32,
        json: true,
        service_name: "zccache".into(),
        output_path: String::new(),
    };
    assert_roundtrip(request);
}

#[test]
fn admin_reply_roundtrip() {
    let reply = AdminReply {
        kind: AdminReplyKind::Json as i32,
        body: r#"{"schema_version":1}"#.into(),
        exit_code: 0,
        content_type: "application/json".into(),
    };
    assert_roundtrip(reply);
}

#[test]
fn cache_manifest_roundtrip() {
    let manifest = CacheManifest {
        manifest_schema_version: 1,
        media_type: "application/vnd.running-process.cache-manifest.v1".into(),
        self_sha256: vec![0; 32],
        host: Some(HostIdentity {
            hostname: "build-runner-1".into(),
            machine_id: "1234abcd".into(),
            boot_id: "boot-id-1".into(),
            fs_dev_id: 0xFE_00,
            namespace_id: "mntns:4026531840".into(),
        }),
        current_operation: Some(Operation {
            kind: OperationKind::OperationIdle as i32,
            started_at_unix_ms: 1,
            expected_done_unix_ms: 0,
        }),
        valid_until_unix_ms: 0,
        service_name: "zccache".into(),
        service_version: "1.11.20".into(),
        broker_envelope_version: "v1".into(),
        created_at_unix_ms: 1_700_000_000_000,
        last_active_unix_ms: 1_700_000_000_500,
        roots: vec![CacheRoot {
            path: "/var/cache/zccache".into(),
            kind: CacheRootKind::CacheData as i32,
            estimated_size_bytes: 1_073_741_824,
            disposition: StorageDisposition::PruneWhenDormant as i32,
            labels: HashMap::new(),
            quota: Some(Quota {
                hard_max_bytes: 10_000_000_000,
                soft_target_bytes: 5_000_000_000,
                reserved_bytes: 1_000_000,
            }),
            teardown_hook: Some(TeardownHook {
                kind: TeardownKind::TeardownRedbCompact as i32,
                argv: vec![],
                timeout_secs: 30,
            }),
            exclude_globs: vec!["**/.tmp".into()],
            platform_paths: HashMap::new(),
            ownership: Some(Ownership {
                uid: 1000,
                gid: 1000,
                mode: 0o700,
                windows_sid: String::new(),
            }),
            endpoint: Some(Endpoint {
                namespace_id: "mntns:4026531840".into(),
                path: r"\\.\pipe\zccache".into(),
            }),
        }],
        current_daemon: Some(DaemonProcess {
            pid: 4321,
            exe_path: "/usr/local/bin/zccache".into(),
            exe_sha256: vec![1; 32],
            ipc_endpoint: Some(Endpoint {
                namespace_id: "mntns:4026531840".into(),
                path: "/tmp/zccache.sock".into(),
            }),
            started_at_unix_ms: 1_700_000_000_000,
            boot_id: "boot-id-1".into(),
            idle_timeout_secs: Some(900),
        }),
        cleanup_policy: Some(CleanupPolicy {
            dormant_after_secs: 30 * 24 * 60 * 60,
            keep_last_n_versions: 2,
            keep_current: true,
            min_size_for_prune_bytes: 64 * 1024,
        }),
        broker_instance: "shared".into(),
        depends_on: vec![ManifestRef {
            service_name: "soldr-daemon".into(),
            min_version: "0.7.0".into(),
            optional: true,
        }],
        provides: vec!["zccache.v1".into()],
        observability: Some(ObservabilityInfo {
            metrics_endpoint: "127.0.0.1:9100/metrics".into(),
            log_path: "/var/log/zccache.log".into(),
            health_check_endpoint: "127.0.0.1:9100/health".into(),
        }),
        bundle_id: "bundle-abc".into(),
    };
    assert_roundtrip(manifest);
}

#[test]
fn service_definition_roundtrip() {
    let mut labels = HashMap::new();
    labels.insert("env".into(), "ci".into());
    let svc = ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: "/usr/local/bin/zccache".into(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: "/opt/zccache/versions".into(),
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into(), "1.12.0".into()],
        labels,
    };
    assert_roundtrip(svc);
}

#[test]
fn lifecycle_event_roundtrip() {
    let mut extra = HashMap::new();
    extra.insert("backend_pid".into(), "4321".into());
    let evt = LifecycleEvent {
        ts_ms: 1_700_000_000_500,
        pid: 4321,
        service_name: "zccache".into(),
        kind: EventKind::Spawn as i32,
        reason: "broker spawn".into(),
        extra,
        severity_number: 9,
        severity_text: "INFO".into(),
        request_id: "req-42".into(),
        connection_id: 99,
        broker_instance: "shared".into(),
    };
    assert_roundtrip(evt);
}
