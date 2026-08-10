#![cfg(feature = "client")]

use running_process::broker::manifest;
use running_process::broker::protocol::CacheManifest;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn manifest_for(version: &str) -> CacheManifest {
    CacheManifest {
        manifest_schema_version: 1,
        media_type: manifest::CACHE_MANIFEST_MEDIA_TYPE.to_string(),
        self_sha256: Vec::new(),
        host: Some(running_process::broker::host_identity::current()),
        current_operation: None,
        valid_until_unix_ms: 0,
        service_name: "zccache".to_string(),
        service_version: version.to_string(),
        broker_envelope_version: "v1".to_string(),
        created_at_unix_ms: 1,
        last_active_unix_ms: 2,
        roots: Vec::new(),
        current_daemon: None,
        cleanup_policy: None,
        broker_instance: "shared".to_string(),
        depends_on: Vec::new(),
        provides: Vec::new(),
        observability: None,
        bundle_id: String::new(),
    }
}

#[test]
fn repeated_write_never_leaves_torn_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    manifest::write_to_root(&root, &manifest_for("1.2.3")).unwrap();
    manifest::write_to_root(&root, &manifest_for("1.2.4")).unwrap();

    let read = manifest::read_manifest(&root.join(manifest::ROOT_MANIFEST_FILE)).unwrap();
    assert_eq!(read.service_version, "1.2.4");
    assert_eq!(read.self_sha256.len(), 32);
}

#[test]
fn interrupted_writer_temp_file_does_not_replace_committed_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let target = root.join(manifest::ROOT_MANIFEST_FILE);
    manifest::write_to_root(&root, &manifest_for("1.2.3")).unwrap();

    let interrupted = root.join(".running-process-manifest.pb.tmp-interrupted");
    let mut file = std::fs::File::create(&interrupted).unwrap();
    file.write_all(b"partial protobuf payload").unwrap();
    file.sync_all().unwrap();

    let read = manifest::read_manifest(&target).unwrap();
    assert_eq!(read.service_version, "1.2.3");

    manifest::write_to_root(&root, &manifest_for("1.2.4")).unwrap();
    let read = manifest::read_manifest(&target).unwrap();
    assert_eq!(read.service_version, "1.2.4");
    assert!(interrupted.exists());
}

#[test]
fn killed_writer_mid_write_keeps_committed_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let target = root.join(manifest::ROOT_MANIFEST_FILE);
    manifest::write_to_root(&root, &manifest_for("1.2.3")).unwrap();

    let ready = root.join("writer-ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("manifest_atomic::killed_writer_child")
        .arg("--nocapture")
        .env("RUNNING_PROCESS_MANIFEST_KILL_WRITER_ROOT", &root)
        .env("RUNNING_PROCESS_MANIFEST_KILL_WRITER_READY", &ready)
        .spawn()
        .unwrap();

    wait_for_path(&ready, Duration::from_secs(10));
    child.kill().unwrap();
    let _ = child.wait().unwrap();

    let read = manifest::read_manifest(&target).unwrap();
    assert_eq!(read.service_version, "1.2.3");

    manifest::write_to_root(&root, &manifest_for("1.2.4")).unwrap();
    let read = manifest::read_manifest(&target).unwrap();
    assert_eq!(read.service_version, "1.2.4");
}

#[test]
fn killed_writer_child() {
    let Some(root) = std::env::var_os("RUNNING_PROCESS_MANIFEST_KILL_WRITER_ROOT") else {
        return;
    };
    let Some(ready) = std::env::var_os("RUNNING_PROCESS_MANIFEST_KILL_WRITER_READY") else {
        return;
    };

    let root = PathBuf::from(root);
    let ready = PathBuf::from(ready);
    std::fs::create_dir_all(&root).unwrap();
    let interrupted = root.join(".running-process-manifest.pb.tmp-killed-writer-child");
    let mut file = std::fs::File::create(interrupted).unwrap();
    let chunk = vec![b'x'; 1024 * 1024];
    file.write_all(&chunk).unwrap();
    file.flush().unwrap();
    std::fs::write(&ready, b"ready").unwrap();

    loop {
        file.write_all(&chunk).unwrap();
        file.flush().unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_path(path: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}
