#[cfg(unix)]
use std::path::PathBuf;

/// One broker instance discovered from the local pipe namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerInstance {
    /// Filesystem path or named pipe string.
    pub path: String,
}

/// Enumerate broker v1 instances visible to the current user.
pub fn list() -> Vec<BrokerInstance> {
    #[cfg(unix)]
    {
        unix_instance_dirs()
            .into_iter()
            .flat_map(|dir| {
                std::fs::read_dir(dir)
                    .into_iter()
                    .flat_map(|rd| rd.flatten())
                    .filter_map(|entry| {
                        let path = entry.path();
                        let name = path.file_name()?.to_string_lossy();
                        if name.starts_with("rpb-v1-") && name.ends_with(".sock") {
                            Some(BrokerInstance {
                                path: path.to_string_lossy().into_owned(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
    #[cfg(windows)]
    {
        // Windows named-pipe namespace enumeration lands with the
        // broker binary in Phase 4. Phase 2 keeps the command stable
        // and returns an empty list when no broker is required.
        Vec::new()
    }
}

/// Render `running-process-cleanup instances --json`.
pub fn render_json(instances: &[BrokerInstance]) -> String {
    let body = instances
        .iter()
        .map(|instance| {
            format!(
                "{{\"path\":\"{}\"}}",
                crate::cleanup::json_escape(&instance.path)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"schema_version\":1,\"instances\":[{body}]}}")
}

#[cfg(unix)]
fn unix_instance_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        dirs.push(
            PathBuf::from(runtime)
                .join("running-process")
                .join("broker"),
        );
    }
    dirs.push(std::env::temp_dir());
    dirs
}
