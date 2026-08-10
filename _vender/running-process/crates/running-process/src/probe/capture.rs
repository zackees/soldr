//! Cooperative capture producer for daemon-leased #637 work.
//!
//! This code runs inside the registered process. That is the only address
//! space where captured absolute addresses can be attributed to loaded
//! modules before ASLR makes them meaningless. The artifact handed to the
//! daemon contains module indexes and relative offsets only.

use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use running_process_probe::probe_diag::v1::{CaptureReply, CaptureStackRequest};
use running_process_probe::snapshot::{
    attribute::{attribute, AttributedCapture},
    capture_and_resolve,
    modules::enumerate_modules,
    SnapshotConfig,
};
use serde::Serialize;

const DEFAULT_MAX_DEPTH: usize = 256;

#[derive(Serialize)]
struct RawCapture {
    format: &'static str,
    discovery: RawDiscoveryConfig,
    modules: Vec<RawModule>,
    threads: Vec<RawThread>,
}

#[derive(Serialize)]
struct RawDiscoveryConfig {
    registered_manifest: Option<String>,
    registered_symbol_paths: Vec<String>,
}

#[derive(Serialize)]
struct RawModule {
    name: String,
    base_avma: u64,
    debug_id: Option<String>,
    debug_file: Option<String>,
    code_id: Option<String>,
    path_hint: Option<String>,
}

#[derive(Serialize)]
struct RawThread {
    os_tid: u64,
    frames: Vec<RawFrame>,
    py_frames: Vec<RawPyFrame>,
}

#[derive(Serialize)]
struct RawFrame {
    module_index: u32,
    relative_address: u64,
}

#[derive(Serialize)]
struct RawPyFrame {
    file: String,
    line: u32,
    func: String,
}

/// Capture this process and return the artifact reference used by the wire.
pub(super) fn capture(request: &CaptureStackRequest) -> CaptureReply {
    let started_unix_ms = unix_millis();
    match capture_inner(request) {
        Ok((path, threads_captured, threads_dropped, pause_ns)) => CaptureReply {
            started_unix_ms,
            artifact_path: path.to_string_lossy().into_owned(),
            threads_captured,
            threads_dropped,
            pause_ns,
            ..Default::default()
        },
        Err(error) => CaptureReply {
            started_unix_ms,
            // PROBE_ERROR_INTERNAL. The daemon records this against the
            // leased job; the application itself stays alive and registered.
            error: 5,
            detail: error.to_string(),
            ..Default::default()
        },
    }
}

fn capture_inner(request: &CaptureStackRequest) -> io::Result<(PathBuf, u32, u32, u64)> {
    let snapshot = capture_and_resolve(&SnapshotConfig::default())
        .map_err(|error| io::Error::other(format!("cooperative capture failed: {error}")))?;
    let stats = snapshot.stats;
    let modules = enumerate_modules()
        .map_err(|error| io::Error::other(format!("module inventory failed: {error}")))?;
    let attributed = attribute(&snapshot, &modules);
    let payload = worker_payload(&attributed, request);
    let path = create_artifact(&payload)?;
    Ok((
        path,
        stats.threads_captured,
        stats.threads_dropped,
        stats.pause_nanos,
    ))
}

fn worker_payload(capture: &AttributedCapture, request: &CaptureStackRequest) -> RawCapture {
    let depth = if request.max_depth == 0 {
        DEFAULT_MAX_DEPTH
    } else {
        request.max_depth as usize
    };
    let modules = capture
        .modules
        .iter()
        .map(|module| RawModule {
            name: module.name.clone(),
            base_avma: module.base,
            debug_id: module.debug_id.clone(),
            debug_file: module.debug_file.clone(),
            code_id: module.path.as_deref().and_then(captured_file_identity),
            path_hint: module.path.clone(),
        })
        .collect();
    let threads = capture
        .threads
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            request.thread_filter == 0
                || (*index < u32::BITS as usize && request.thread_filter & (1 << *index) != 0)
        })
        .map(|(_, thread)| RawThread {
            os_tid: thread.os_tid,
            frames: thread
                .frames
                .iter()
                .take(depth)
                .map(|frame| RawFrame {
                    // An out-of-range index is the worker wire's explicit
                    // ModuleUnknown representation. Never guess an owner.
                    module_index: frame.module_index.unwrap_or(u32::MAX),
                    relative_address: frame.relative_address,
                })
                .collect(),
            // Native registrations have no interpreter. Python's separate
            // mixed-mode producer populates this same field before invoking
            // the worker; an empty list is explicit rather than omitted.
            py_frames: Vec::new(),
        })
        .collect();
    RawCapture {
        format: "cooperative_frames",
        discovery: RawDiscoveryConfig {
            registered_manifest: (!request.symbol_manifest_path.is_empty())
                .then(|| request.symbol_manifest_path.clone()),
            registered_symbol_paths: request.symbol_paths.clone(),
        },
        modules,
        threads,
    }
}

const MAX_CAPTURE_IDENTITY_BYTES: u64 = 512 * 1024 * 1024;

fn captured_file_identity(path: &str) -> Option<String> {
    let path = std::path::Path::new(path);
    let Ok(metadata) = std::fs::metadata(path) else {
        return Some("sha256:unavailable".to_owned());
    };
    if metadata.len() > MAX_CAPTURE_IDENTITY_BYTES {
        // An explicit unusable identity makes the worker refuse this image.
        // Omitting the field would let it trust a path that may be replaced
        // after the target resumes.
        return Some("sha256:unavailable".to_owned());
    }
    let Ok(digest) = crate::broker::backend_lifecycle::identity::sha256_file(path) else {
        return Some("sha256:unavailable".to_owned());
    };
    Some(format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn create_artifact(payload: &RawCapture) -> io::Result<PathBuf> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let token = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path = std::env::temp_dir().join(format!(
        "rp-probe-capture-{}-{token}.json",
        std::process::id()
    ));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    serde_json::to_writer(&mut file, payload)
        .map_err(|error| io::Error::other(format!("encode capture: {error}")))?;
    file.flush()?;
    Ok(path)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process_probe::snapshot::attribute::{
        AttributedFrame, AttributedModule, AttributedThread,
    };

    #[test]
    fn worker_payload_is_bounded_and_keeps_unknown_frames_explicit() {
        let capture = AttributedCapture {
            modules: vec![AttributedModule {
                name: "app.exe".into(),
                path: Some("app.exe".into()),
                debug_id: Some("pdb:00112233445566778899aabbccddeeff-1".into()),
                debug_file: Some("app-recorded.pdb".into()),
                base: 0x1000,
            }],
            threads: vec![AttributedThread {
                os_tid: 7,
                frames: vec![
                    AttributedFrame {
                        module_index: Some(0),
                        relative_address: 0x10,
                    },
                    AttributedFrame {
                        module_index: None,
                        relative_address: 0xDEAD,
                    },
                ],
            }],
        };
        let request = CaptureStackRequest {
            max_depth: 2,
            symbol_manifest_path: "test.symbols.json".into(),
            symbol_paths: vec!["symbols".into()],
            ..Default::default()
        };
        let payload = worker_payload(&capture, &request);
        assert_eq!(payload.threads.len(), 1);
        assert_eq!(payload.threads[0].frames[0].module_index, 0);
        assert_eq!(payload.threads[0].frames[1].module_index, u32::MAX);
        assert_eq!(payload.threads[0].frames[1].relative_address, 0xDEAD);
        assert_eq!(
            payload.modules[0].debug_id.as_deref(),
            Some("pdb:00112233445566778899aabbccddeeff-1")
        );
        assert_eq!(
            payload.modules[0].debug_file.as_deref(),
            Some("app-recorded.pdb")
        );
        assert_eq!(
            payload.discovery.registered_manifest.as_deref(),
            Some("test.symbols.json")
        );
        assert_eq!(payload.discovery.registered_symbol_paths, vec!["symbols"]);
    }

    #[test]
    fn module_file_identity_is_captured_before_the_artifact_leaves_the_target() {
        let exe = std::env::current_exe().unwrap();
        let identity = captured_file_identity(&exe.to_string_lossy()).expect("sha256 identity");
        assert!(identity.starts_with("sha256:"));
        assert_eq!(identity.len(), "sha256:".len() + 64);
    }
}
