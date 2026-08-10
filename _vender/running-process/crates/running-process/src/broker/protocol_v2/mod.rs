//! v2 broker protocol module.
//!
//! Houses the prost-generated types for the `running_process.broker.v2`
//! package — currently the `ServiceDefinition` envelope and the
//! `HttpServerCapability` optional sub-message introduced in #483.
//!
//! v2 runs in parallel with v1 (`super::protocol`) through the broker
//! v2 rollout; v1's types are FROZEN FOREVER (#228) so all new
//! capability fields land here instead.

#[allow(missing_docs)]
mod prost_generated {
    include!(concat!(env!("OUT_DIR"), "/running_process.broker.v2.rs"));
}

pub use prost_generated::*;

mod io;
pub use io::{
    service_definition_dir_v2, service_definition_path_v2, write_service_definition_v2,
    ServiceDefinitionBuilder, SERVICE_DEF_V2_EXTENSION,
};

mod manifest_io;
pub use manifest_io::{
    central_manifest_path_v2, central_registry_dir_v2, write_to_central_in_dir_v2,
    write_to_central_v2, write_to_root_v2, CacheManifestBuilder, BROKER_ENVELOPE_VERSION_V2,
    CENTRAL_MANIFEST_EXTENSION_V2, ROOT_MANIFEST_FILE_V2,
};

pub mod backend_handle;

pub mod client_compat;

mod loader;
pub use loader::{ServiceDefinitionLoader, ServiceDefinitionScanEntry};

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    /// `ServiceDefinition` round-trips with no HTTP capability — the
    /// optional field is absent on both sides.
    #[test]
    fn service_definition_without_http_round_trips() {
        let original = ServiceDefinition {
            service_name: "zccache".to_owned(),
            http_server: None,
            ..Default::default()
        };

        let bytes = original.encode_to_vec();
        let decoded =
            ServiceDefinition::decode(bytes.as_slice()).expect("encoded ServiceDefinition decodes");

        assert_eq!(decoded.service_name, "zccache");
        assert!(decoded.http_server.is_none());
    }

    /// `ServiceDefinition` round-trips with an `HttpServerCapability`
    /// populated — all three fields survive.
    #[test]
    fn service_definition_with_http_round_trips() {
        let original = ServiceDefinition {
            service_name: "fbuild".to_owned(),
            http_server: Some(HttpServerCapability {
                bind_addr: "127.0.0.1".to_owned(),
                health_path: "/healthz".to_owned(),
                display_name: "fbuild status".to_owned(),
            }),
            ..Default::default()
        };

        let bytes = original.encode_to_vec();
        let decoded =
            ServiceDefinition::decode(bytes.as_slice()).expect("encoded ServiceDefinition decodes");

        let cap = decoded
            .http_server
            .expect("http_server survives round-trip");
        assert_eq!(decoded.service_name, "fbuild");
        assert_eq!(cap.bind_addr, "127.0.0.1");
        assert_eq!(cap.health_path, "/healthz");
        assert_eq!(cap.display_name, "fbuild status");
    }

    /// Empty `HttpServerCapability` survives a round-trip — defaults are
    /// applied by the loader/consumer, not by the proto encoder.
    #[test]
    fn http_server_capability_empty_defaults_survive_round_trip() {
        let original = ServiceDefinition {
            service_name: "minimal".to_owned(),
            http_server: Some(HttpServerCapability::default()),
            ..Default::default()
        };

        let bytes = original.encode_to_vec();
        let decoded =
            ServiceDefinition::decode(bytes.as_slice()).expect("encoded ServiceDefinition decodes");

        let cap = decoded
            .http_server
            .expect("http_server survives round-trip");
        assert!(cap.bind_addr.is_empty());
        assert!(cap.health_path.is_empty());
        assert!(cap.display_name.is_empty());
    }

    /// Slice 22 (zackees/zccache#782): the launcher / isolation fields
    /// ported from v1 round-trip cleanly. Pins every new field plus the
    /// `BrokerIsolation` enum mapping so a future proto regression
    /// surfaces here instead of at the first downstream loader.
    #[test]
    fn service_definition_v1_fields_round_trip() {
        use std::collections::HashMap;
        let mut labels = HashMap::new();
        labels.insert("env".to_owned(), "prod".to_owned());
        labels.insert("deploy".to_owned(), "blue".to_owned());

        let original = ServiceDefinition {
            service_name: "zccache".to_owned(),
            binary_path: "/usr/local/bin/zccache-daemon".to_owned(),
            isolation: BrokerIsolation::SharedBroker as i32,
            explicit_instance: String::new(),
            per_version_binary_dir: "/usr/local/bin".to_owned(),
            min_version: "1.0.0".to_owned(),
            version_allow_list: vec!["1.12.9".to_owned(), "1.13.0".to_owned()],
            labels,
            http_server: None,
        };

        let bytes = original.encode_to_vec();
        let decoded =
            ServiceDefinition::decode(bytes.as_slice()).expect("encoded ServiceDefinition decodes");

        assert_eq!(decoded.service_name, "zccache");
        assert_eq!(decoded.binary_path, "/usr/local/bin/zccache-daemon");
        assert_eq!(decoded.isolation, BrokerIsolation::SharedBroker as i32);
        assert!(decoded.explicit_instance.is_empty());
        assert_eq!(decoded.per_version_binary_dir, "/usr/local/bin");
        assert_eq!(decoded.min_version, "1.0.0");
        assert_eq!(
            decoded.version_allow_list,
            vec!["1.12.9".to_owned(), "1.13.0".to_owned()]
        );
        assert_eq!(decoded.labels.len(), 2);
        assert_eq!(decoded.labels.get("env"), Some(&"prod".to_owned()));
        assert_eq!(decoded.labels.get("deploy"), Some(&"blue".to_owned()));
        assert!(decoded.http_server.is_none());
    }

    /// Slice 22 (zackees/zccache#782): every `BrokerIsolation` enum
    /// variant survives the round-trip. Pins the proto-int mapping so
    /// future variant additions get an explicit failure here instead
    /// of misclassifying as `PrivateBroker` (the proto3 zero value).
    #[test]
    fn broker_isolation_enum_values_round_trip() {
        for iso in [
            BrokerIsolation::PrivateBroker,
            BrokerIsolation::SharedBroker,
            BrokerIsolation::ExplicitInstance,
        ] {
            let original = ServiceDefinition {
                service_name: format!("svc-{}", iso as i32),
                isolation: iso as i32,
                ..Default::default()
            };
            let bytes = original.encode_to_vec();
            let decoded = ServiceDefinition::decode(bytes.as_slice())
                .expect("encoded ServiceDefinition decodes");
            assert_eq!(decoded.isolation, iso as i32, "round-trip of {iso:?}");
        }
    }

    /// Slice 22: `explicit_instance` is only meaningful when
    /// `isolation == ExplicitInstance`, but the proto encoder doesn't
    /// enforce the gating — the consumer (broker / loader) does. Pin
    /// that the field round-trips regardless of the isolation value
    /// so a future broker policy change can rely on the bytes round-tripping
    /// faithfully even for "invalid" combinations.
    #[test]
    fn service_definition_explicit_instance_round_trips_with_any_isolation() {
        for iso in [
            BrokerIsolation::PrivateBroker,
            BrokerIsolation::SharedBroker,
            BrokerIsolation::ExplicitInstance,
        ] {
            let original = ServiceDefinition {
                service_name: "svc".to_owned(),
                isolation: iso as i32,
                explicit_instance: "ci-trusted".to_owned(),
                ..Default::default()
            };
            let bytes = original.encode_to_vec();
            let decoded = ServiceDefinition::decode(bytes.as_slice())
                .expect("encoded ServiceDefinition decodes");
            assert_eq!(
                decoded.explicit_instance, "ci-trusted",
                "explicit_instance must survive round-trip even with isolation={iso:?}"
            );
        }
    }

    /// `BackendHttpReady` carries the daemon's OS-allocated port back to
    /// the broker; encodes/decodes without loss.
    #[test]
    fn backend_http_ready_round_trips() {
        let original = BackendHttpReady { port: 49_152 };

        let bytes = original.encode_to_vec();
        let decoded = BackendHttpReady::decode(bytes.as_slice()).expect("BackendHttpReady decodes");

        assert_eq!(decoded.port, 49_152);
    }

    /// `GetBrokerHttpEndpointRequest` is an empty marker; encoding +
    /// decoding it produces the same default-constructed message.
    #[test]
    fn get_broker_http_endpoint_request_round_trips_empty() {
        let original = GetBrokerHttpEndpointRequest::default();

        let bytes = original.encode_to_vec();
        let decoded = GetBrokerHttpEndpointRequest::decode(bytes.as_slice())
            .expect("GetBrokerHttpEndpointRequest decodes");

        assert_eq!(decoded, GetBrokerHttpEndpointRequest::default());
    }

    /// `GetBrokerHttpEndpointResponse` round-trips both fields (port + pid).
    #[test]
    fn get_broker_http_endpoint_response_round_trips() {
        let original = GetBrokerHttpEndpointResponse {
            port: 8765,
            pid: 12_345,
        };

        let bytes = original.encode_to_vec();
        let decoded = GetBrokerHttpEndpointResponse::decode(bytes.as_slice())
            .expect("GetBrokerHttpEndpointResponse decodes");

        assert_eq!(decoded.port, 8765);
        assert_eq!(decoded.pid, 12_345);
    }
}
