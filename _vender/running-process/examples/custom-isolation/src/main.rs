use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use running_process::broker::lifecycle::{explicit_instance_pipe, user_sid_hash};
use running_process::broker::protocol::{BrokerIsolation, CacheManifest, ServiceDefinition};

fn main() -> Result<(), Box<dyn Error>> {
    let instance =
        env::var("RUNNING_PROCESS_EXPLICIT_INSTANCE").unwrap_or_else(|_| "ci-trusted".to_string());
    let user_hash = user_sid_hash()?;
    let pipe = explicit_instance_pipe(&user_hash, &instance)?;
    let endpoint = platform_pipe_string(pipe);
    let binary_path = env::current_exe()?;
    let binary_dir = binary_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?);

    let service_definition = ServiceDefinition {
        service_name: "zccache".to_string(),
        binary_path: binary_path.to_string_lossy().into_owned(),
        isolation: BrokerIsolation::ExplicitInstance as i32,
        explicit_instance: instance.clone(),
        per_version_binary_dir: binary_dir.to_string_lossy().into_owned(),
        min_version: "1.0.0".to_string(),
        labels: HashMap::from([
            ("trust-group".to_string(), instance.clone()),
            ("owner".to_string(), "ci".to_string()),
        ]),
        ..Default::default()
    };

    let now = unix_now_ms();
    let manifest = CacheManifest {
        host: Some(running_process::broker::host_identity::current()),
        service_name: service_definition.service_name.clone(),
        service_version: service_definition.min_version.clone(),
        broker_envelope_version: "v1".to_string(),
        created_at_unix_ms: now,
        last_active_unix_ms: now,
        valid_until_unix_ms: now + 30_000,
        broker_instance: service_definition.explicit_instance.clone(),
        bundle_id: format!("{}-{}", service_definition.service_name, instance),
        ..Default::default()
    };

    println!(
        "service={} isolation={:?} instance={} endpoint={} manifest_instance={}",
        service_definition.service_name,
        BrokerIsolation::try_from(service_definition.isolation)?,
        service_definition.explicit_instance,
        endpoint,
        manifest.broker_instance
    );

    Ok(())
}

fn platform_pipe_string(pipe: running_process::broker::lifecycle::PipePath) -> String {
    #[cfg(windows)]
    {
        pipe.windows.expect("windows pipe path is populated")
    }
    #[cfg(unix)]
    {
        pipe.unix
            .expect("unix socket path is populated")
            .to_string_lossy()
            .into_owned()
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
