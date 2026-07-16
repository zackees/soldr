mod common;

use common::*;
use soldr_cli::timed_test;
use std::path::Path;

fn fake_global_soldr(log: &Path) -> String {
    #[cfg(windows)]
    {
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
    }
    #[cfg(not(windows))]
    {
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

timed_test!(project_policy_delegates_to_newer_global_soldr, {
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
});
