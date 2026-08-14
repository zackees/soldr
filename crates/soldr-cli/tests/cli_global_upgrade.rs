mod common;

use common::*;
use std::path::Path;

fn fake_global_soldr(log: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             if \"%~1\"==\"--version\" (\n\
               echo soldr 999.0.0\n\
               exit /b 0\n\
             )\n\
             echo %*>>\"{}\"\n\
             exit /b 73\n",
            log.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then\n\
               echo 'soldr 999.0.0'\n\
               exit 0\n\
             fi\n\
             printf '%s\\n' \"$*\" >> \"{}\"\n\
             exit 73\n",
            log.display()
        )
    }
}

#[test]
fn project_policy_delegates_to_newer_global_soldr() {
    let fixture = unique_temp_dir("newer-global-soldr");
    std::fs::write(
        fixture.join("Cargo.toml"),
        "[workspace]\n\n[workspace.metadata.soldr]\nprefer_newer_global = true\n",
    )
    .expect("write opt-in manifest");

    let global_bin_dir = fixture.join("global-bin");
    std::fs::create_dir_all(&global_bin_dir).expect("create global bin dir");
    let log = fixture.join("global-invocation.log");
    let global_soldr = fake_script_path(&global_bin_dir, "soldr");
    write_fake_script(&global_soldr, &fake_global_soldr(&log));

    let output = isolated_soldr_command()
        .arg("status")
        .current_dir(&fixture)
        .env("PATH", prepend_to_path(&global_bin_dir))
        .env("SOLDR_CACHE_DIR", fixture.join("cache"))
        .output()
        .expect("run local soldr");

    assert_eq!(
        output.status.code(),
        Some(73),
        "newer global soldr must own the invocation\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&log)
            .expect("read delegated invocation")
            .trim(),
        "status"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("delegating to newer global soldr"),
        "delegation should be visible on stderr"
    );
}

// The delegation policy must NOT apply to `RUSTC_WRAPPER` callbacks
// (#1847). Cargo issues one per compile unit, and `probe_version`
// spawns the global soldr to read its `--version` — 52-61 ms measured,
// which dominated the wrapper hot path. Delegating here would also swap
// the compiler wrapper (and its daemon/cache peer) mid-build.
//
// Companion to `project_policy_delegates_to_newer_global_soldr` above:
// that asserts a user-facing invocation still hands off, this asserts a
// wrapper invocation never does. Same opt-in manifest, same newer fake
// global on PATH — only the invocation shape differs.
#[test]
fn wrapper_invocations_never_delegate_to_newer_global_soldr() {
    let fixture = unique_temp_dir("wrapper-no-global-delegate");
    std::fs::write(
        fixture.join("Cargo.toml"),
        "[workspace]\n\n[workspace.metadata.soldr]\nprefer_newer_global = true\n",
    )
    .expect("write opt-in manifest");

    let global_bin_dir = fixture.join("global-bin");
    std::fs::create_dir_all(&global_bin_dir).expect("create global bin dir");
    let log = fixture.join("global-invocation.log");
    let global_soldr = fake_script_path(&global_bin_dir, "soldr");
    write_fake_script(&global_soldr, &fake_global_soldr(&log));

    // A wrapper invocation is `soldr <rustc-like-path> <args>`; the stem
    // is what `is_wrapper_invocation` matches on. `--print sysroot` is a
    // non-cacheable probe, so it passes straight through to this stub
    // without involving the daemon.
    let fake_rustc = fake_script_path(&fixture, "rustc");
    let body = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "@echo off\necho fake-sysroot\nexit /b 0\n"
    } else {
        "#!/bin/sh\necho fake-sysroot\nexit 0\n"
    };
    write_fake_script(&fake_rustc, body);

    let output = isolated_soldr_command()
        .arg(&fake_rustc)
        .arg("--print")
        .arg("sysroot")
        .current_dir(&fixture)
        .env("PATH", prepend_to_path(&global_bin_dir))
        .env("SOLDR_CACHE_DIR", fixture.join("cache"))
        .output()
        .expect("run local soldr in wrapper mode");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(
        output.status.code(),
        Some(73),
        "wrapper invocation must not hand off to the global soldr\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !log.exists(),
        "global soldr must not be invoked from a wrapper callback; log:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    assert!(
        !stderr.contains("delegating to newer global soldr"),
        "wrapper invocation should not announce delegation\nstderr:\n{stderr}"
    );
}
