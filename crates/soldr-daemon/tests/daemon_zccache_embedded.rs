//! Embedded zccache tests: the broken-symlink legacy-sweep
//! retention check and the executable fake-compiler probe. Moved out of
//! `zccache_embedded.rs` when that file became host-neutral (#2493);
//! both depend on Unix link/permission semantics.

use std::time::Duration;

use soldr_daemon::core::SoldrPaths;
use soldr_daemon::zccache_embedded::sweep_legacy_cache_roots;

#[test]
fn legacy_sweep_retains_version_with_unreadable_linked_tree() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let candidate = paths.cache.join("zccache/v0.0.1");
    std::fs::create_dir_all(&candidate).unwrap();
    soldr_platform::fs::links::create("missing", &candidate.join("broken"), false).unwrap();
    let report = sweep_legacy_cache_roots(&paths, std::time::SystemTime::now(), Duration::ZERO);
    assert_eq!(report.removed, 0);
    assert_eq!(report.failed, 1);
    assert!(candidate.is_dir());
}

#[test]
fn working_fake_compiler_probe_is_accepted() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let compiler = temp.path().join("fake-compiler");
    std::fs::write(
        &compiler,
        "#!/bin/sh\nprintf 'rustc 1.94.1 (fake)\\nhost: fake-target\\n'\n",
    )
    .expect("write fake compiler");
    soldr_platform::fs::permissions::make_executable(&compiler)
        .expect("make fake compiler executable");

    let version = probe_working_compiler(&compiler).expect("working compiler probe");
    assert!(version.contains("rustc 1.94.1 (fake)"));
}

/// Run `rustc -vV` against `path` and require plausible rustc output,
/// mirroring the in-module probe that `zccache_embedded.rs` uses.
fn probe_working_compiler(path: &std::path::Path) -> Result<String, String> {
    let mut command = std::process::Command::new(path);
    command.arg("-vV");
    let output =
        running_process::run_std_command_bounded(command, Some(Duration::from_secs(30)), 64 * 1024)
            .map_err(|error| format!("probe failed: path={} error={error}", path.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.exit_code != 0 {
        return Err(format!(
            "path={} exit_code={:?}\nstdout:\n{}\nstderr:\n{}",
            path.display(),
            Some(output.exit_code),
            stdout,
            stderr
        ));
    }
    let version = stdout.trim();
    let mut lines = version.lines();
    let has_rustc_version = lines.next().is_some_and(|line| line.starts_with("rustc "));
    let has_host = lines.any(|line| line.starts_with("host: "));
    if !has_rustc_version || !has_host {
        return Err(format!(
            "path={} unexpected rustc -vV output\nstdout:\n{}\nstderr:\n{}",
            path.display(),
            stdout,
            stderr
        ));
    }
    Ok(version.to_string())
}
