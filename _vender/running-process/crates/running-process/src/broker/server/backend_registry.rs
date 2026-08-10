//! Verified backend registry keyed by broker instance, service, and version.

use std::collections::HashMap;

use crate::broker::backend_handle::BackendHandle;
use crate::broker::protocol::ServiceDefinition;
use crate::broker::server::hello_handler::RegisteredBackend;
use crate::broker::server::instance::BrokerInstanceKey;

/// Lookup key for one backend process.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BackendKey {
    /// Broker trust-domain instance.
    pub instance: BrokerInstanceKey,
    /// Logical service name.
    pub service_name: String,
    /// Service version.
    pub service_version: String,
}

impl BackendKey {
    /// Build a key from an instance and service tuple.
    pub fn new(
        instance: BrokerInstanceKey,
        service_name: impl Into<String>,
        service_version: impl Into<String>,
    ) -> Self {
        Self {
            instance,
            service_name: service_name.into(),
            service_version: service_version.into(),
        }
    }
}

/// In-memory table of verified backend handles.
#[derive(Default)]
pub struct BackendRegistry {
    entries: HashMap<BackendKey, BackendHandle>,
}

impl BackendRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Number of registered backend handles.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return true when the registry has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert or replace one verified backend handle.
    pub fn insert(
        &mut self,
        instance: BrokerInstanceKey,
        handle: BackendHandle,
    ) -> Option<BackendHandle> {
        let key = BackendKey::new(
            instance,
            handle.service_name.clone(),
            handle.service_version.clone(),
        );
        self.entries.insert(key, handle)
    }

    /// Return one handle by exact instance/service/version key.
    pub fn get(
        &self,
        instance: &BrokerInstanceKey,
        service_name: &str,
        service_version: &str,
    ) -> Option<&BackendHandle> {
        self.entries.get(&BackendKey::new(
            instance.clone(),
            service_name,
            service_version,
        ))
    }

    /// Iterate over all registered backend handles.
    pub fn iter(&self) -> impl Iterator<Item = (&BackendKey, &BackendHandle)> {
        self.entries.iter()
    }

    /// Remove backend handles whose verified process is no longer alive.
    ///
    /// Returns the removed keys so the lifecycle monitor can emit events,
    /// metrics, or diagnostics after the registry mutation is complete.
    pub fn prune_stale(&mut self) -> Vec<BackendKey> {
        let mut removed = Vec::new();
        self.entries.retain(|key, handle| {
            let alive = handle.is_alive();
            if !alive {
                removed.push(key.clone());
            }
            alive
        });
        removed
    }

    /// Return Hello negotiation metadata for one registered backend.
    pub fn registered_backend_for(
        &self,
        instance: &BrokerInstanceKey,
        service_definition: &ServiceDefinition,
        service_version: &str,
    ) -> Option<RegisteredBackend> {
        let handle = self.get(instance, &service_definition.service_name, service_version)?;
        Some(RegisteredBackend {
            service_definition: service_definition.clone(),
            daemon_version: handle.service_version.clone(),
            backend_pipe: handle.daemon_process.ipc_endpoint.path.clone(),
            server_capabilities: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::broker::backend_handle::{BackendHandle, DaemonProcess};
    use crate::broker::protocol::Endpoint;

    use super::*;

    fn handle(service_name: &str, version: &str, pid: u32) -> BackendHandle {
        let endpoint = Endpoint {
            namespace_id: "shared".into(),
            path: format!("rpb-v1-test-{service_name}-{version}"),
        };
        let mut daemon = DaemonProcess::current_process(endpoint, Some(30)).unwrap();
        daemon.pid = pid;

        BackendHandle {
            service_name: service_name.into(),
            service_version: version.into(),
            daemon_process: daemon,
            #[cfg(unix)]
            pid_handle: None,
            #[cfg(windows)]
            process_handle: None,
        }
    }

    #[test]
    fn prune_stale_removes_dead_handles_and_keeps_live_ones() {
        let mut registry = BackendRegistry::new();
        let live_key = BackendKey::new(BrokerInstanceKey::Shared, "zccache", "1.11.20");
        let dead_key = BackendKey::new(BrokerInstanceKey::Shared, "zccache", "1.11.21");

        registry.insert(
            live_key.instance.clone(),
            handle(
                &live_key.service_name,
                &live_key.service_version,
                std::process::id(),
            ),
        );
        registry.insert(
            dead_key.instance.clone(),
            handle(&dead_key.service_name, &dead_key.service_version, u32::MAX),
        );

        let removed = registry.prune_stale();

        assert_eq!(removed, vec![dead_key.clone()]);
        assert!(registry
            .get(
                &live_key.instance,
                &live_key.service_name,
                &live_key.service_version
            )
            .is_some());
        assert!(registry
            .get(
                &dead_key.instance,
                &dead_key.service_name,
                &dead_key.service_version
            )
            .is_none());
    }
}
