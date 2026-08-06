#[cfg(test)]
mod daemon_spawn_image_tests {
    use crate::core::SoldrPaths;
    use crate::daemon::lifecycle::*;
    use tempfile::TempDir;

    crate::timed_test!(daemon_spawn_env_contains_only_soldr_forwarding, {
        let env = daemon_spawn_env();
        let marker = std::ffi::OsString::from(running_process::DAEMON_MARKER_ENV_VAR);
        assert!(
            env.iter().all(|(name, _)| *name != marker),
            "running-process, not Soldr, owns the daemon declaration"
        );
    });

    // The declaration is an overlay on top of the scrub, not a replacement
    // for it: a spawn that declared itself but lost SOLDR_CACHE_DIR would
    // start a daemon pointed at the wrong cache root.
    crate::timed_test!(daemon_spawn_env_keeps_the_forwarded_scrub_survivors, {
        let forwarded = forwarded_soldr_env();
        let spawn_env = daemon_spawn_env();
        for pair in &forwarded {
            assert!(
                spawn_env.contains(pair),
                "{:?} was forwarded but dropped from the daemon spawn env",
                pair.0
            );
        }
        assert_eq!(spawn_env, forwarded);
    });

    // soldr#1931 was not "someone forgot a name" -- it was that nothing tied
    // the resolver's inputs to the spawn allowlist, so #1902 could add a tier
    // the daemon could never see and still land with a green suite and a
    // checked-off "compat path tested" box.
    //
    // This asserts the invariant directly: every env var resolved inside the
    // daemon process must survive the scrub. Adding a daemon-side setting
    // without forwarding it now fails here instead of silently resolving to
    // the default in production.
    crate::timed_test!(every_env_var_read_inside_the_daemon_survives_the_scrub, {
        use crate::core::jobs::{SOLDR_JOBS_ENV_VAR, ZCCACHE_MAX_PARALLEL_COMPILES_ENV_VAR};
        for name in [
            SOLDR_JOBS_ENV_VAR,
            ZCCACHE_MAX_PARALLEL_COMPILES_ENV_VAR,
            zccache::core::config::STAGING_DIR_ENV,
        ] {
            let upper = name.to_ascii_uppercase();
            let forwarded = upper.starts_with(FORWARDED_ENV_PREFIX)
                || FORWARDED_ZCCACHE_ENV.contains(&upper.as_str());
            assert!(
                forwarded,
                concat!(
                    "{} is read by core::jobs inside the daemon, but is scrubbed at ",
                    "the spawn boundary, so that resolver tier can never fire on the ",
                    "auto-spawn path. Add it to FORWARDED_ZCCACHE_ENV."
                ),
                name
            );
        }
    });

    crate::timed_test!(
        forwarded_env_keeps_soldr_namespace_and_only_daemon_read_zccache_vars,
        {
            use std::ffi::OsString;
            let vars = vec![
                (
                    OsString::from("SOLDR_CACHE_DIR"),
                    OsString::from("/tmp/ci-root"),
                ),
                (OsString::from("SOLDR_TRUST_MODE"), OsString::from("strict")),
                (OsString::from("PATH"), OsString::from("/usr/bin")),
                (OsString::from("HOME"), OsString::from("/home/runner")),
                // Dropped: the caller consumes it before any daemon exists.
                (OsString::from("ZCCACHE_DISABLE"), OsString::from("1")),
                // soldr#1931 -- forwarded: `core::jobs` reads this name in the
                // daemon's own process, so scrubbing it makes that resolver
                // tier unreachable on the auto-spawn path.
                (
                    OsString::from("ZCCACHE_MAX_PARALLEL_COMPILES"),
                    OsString::from("6"),
                ),
                (
                    OsString::from("ZCCACHE_STAGING_DIR"),
                    OsString::from("C:\\short-staging"),
                ),
                (OsString::from("soldr_lowercase"), OsString::from("kept")),
                (
                    OsString::from("SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH"),
                    OsString::from("/tmp/daemon.tokio"),
                ),
                (
                    OsString::from("TOKIO_CONSOLE_RECORD_PATH"),
                    OsString::from("/tmp/not-forwarded.tokio"),
                ),
                (
                    OsString::from("zccache_inner_trace"),
                    OsString::from("/tmp/context-registration.jsonl"),
                ),
            ];
            let forwarded = filter_forwarded_env(vars);
            assert_eq!(
                forwarded,
                vec![
                    (
                        OsString::from("SOLDR_CACHE_DIR"),
                        OsString::from("/tmp/ci-root"),
                    ),
                    (OsString::from("SOLDR_TRUST_MODE"), OsString::from("strict")),
                    (
                        OsString::from("ZCCACHE_MAX_PARALLEL_COMPILES"),
                        OsString::from("6"),
                    ),
                    (
                        OsString::from("ZCCACHE_STAGING_DIR"),
                        OsString::from("C:\\short-staging"),
                    ),
                    (OsString::from("soldr_lowercase"), OsString::from("kept")),
                    (
                        OsString::from("SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH"),
                        OsString::from("/tmp/daemon.tokio"),
                    ),
                    (
                        OsString::from("zccache_inner_trace"),
                        OsString::from("/tmp/context-registration.jsonl"),
                    ),
                ]
            );
        }
    );

    crate::timed_test!(detached_spawn_args_preserve_requested_idle_timeout, {
        assert_eq!(
            detached_spawn_args(false, Some(7)),
            ["--foreground", "--idle-timeout-secs", "7"]
        );
        assert_eq!(
            detached_spawn_args(true, Some(0)),
            ["daemon", "start", "--foreground", "--idle-timeout", "0"]
        );
        assert_eq!(detached_spawn_args(false, None), ["--foreground"]);
    });

    #[cfg(unix)]
    crate::timed_test!(via_self_daemon_forces_main_cli_argv0, {
        let mut command = std::process::Command::new("/bin/sh");
        force_daemon_via_self_cli_identity(&mut command);
        command.args(["-c", "printf %s \"$0\""]);
        let output = running_process::run_std_command_bounded(
            command,
            Some(std::time::Duration::from_secs(10)),
            64 * 1024,
        )
        .expect("run shell probe");
        assert_eq!(output.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&output.stdout), "soldr");
    });

    crate::timed_test!(detached_spawn_args_preserve_explicit_idle_timeout, {
        assert_eq!(
            detached_spawn_args(false, Some(60)),
            ["--foreground", "--idle-timeout-secs", "60"]
        );
        assert_eq!(
            detached_spawn_args(true, Some(60)),
            ["daemon", "start", "--foreground", "--idle-timeout", "60"]
        );
        assert_eq!(detached_spawn_args(false, None), ["--foreground"]);
    });

    // #1516 regression: a via-self daemon (no sibling `soldr-daemon`
    // binary) must NOT exec the invoking soldr binary in place — its
    // image must live under the daemon runtime root so the installed
    // binary can be deleted/replaced while the daemon is alive.
    crate::timed_test!(
        via_self_daemon_image_is_relocated_off_the_invoking_binary,
        {
            let temp = TempDir::new().expect("tempdir");
            let install_dir = temp.path().join("Scripts");
            std::fs::create_dir_all(&install_dir).expect("install dir");
            let installed_soldr = install_dir.join("soldr.exe");
            std::fs::write(&installed_soldr, b"installed-soldr").expect("write soldr");
            let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));

            let image = resolve_daemon_spawn_image(Some(&paths), &installed_soldr);

            assert_ne!(
                image, installed_soldr,
                "via-self daemon must not pin the invoking binary"
            );
            assert!(
                !image.starts_with(&install_dir),
                "daemon image {} must not live in the install dir {}",
                image.display(),
                install_dir.display()
            );
            assert!(
                image.starts_with(crate::self_relocate::daemon_runtime_root(&paths)),
                "daemon image {} must live under the daemon runtime root",
                image.display()
            );
            assert_eq!(
                std::fs::read(&image).expect("read relocated image"),
                b"installed-soldr",
                "relocated image must be a byte-identical copy"
            );
        }
    );

    // soldr#1300 constraint: maturin-repaired wheel layouts keep
    // running in place — the via-self relocation must not break them.
    crate::timed_test!(via_self_daemon_in_repaired_wheel_layout_runs_in_place, {
        let temp = TempDir::new().expect("tempdir");
        let scripts = temp.path().join("site-packages").join("soldr.scripts");
        std::fs::create_dir_all(&scripts).expect("scripts dir");
        std::fs::create_dir_all(temp.path().join("site-packages").join("soldr.dylibs"))
            .expect("dylibs dir");
        let wheel_soldr = scripts.join("soldr");
        std::fs::write(&wheel_soldr, b"wheel-soldr").expect("write soldr");
        let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));

        let image = resolve_daemon_spawn_image(Some(&paths), &wheel_soldr);
        assert_eq!(
            image, wheel_soldr,
            "repaired-wheel binaries must run in place (soldr#1300)"
        );
    });

    // Without a resolvable cache root the source runs in place.
    crate::timed_test!(daemon_image_runs_in_place_without_cache_root, {
        let temp = TempDir::new().expect("tempdir");
        let src = temp.path().join("soldr.exe");
        std::fs::write(&src, b"soldr").expect("write soldr");
        assert_eq!(resolve_daemon_spawn_image(None, &src), src);
    });

    crate::timed_test!(configured_daemon_image_requires_canonical_identity, {
        let temp = TempDir::new().expect("tempdir");
        let canonical = temp
            .path()
            .join(format!("soldr-daemon{}", std::env::consts::EXE_SUFFIX));
        let compiler_shim = temp
            .path()
            .join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&canonical, b"soldr").expect("write canonical daemon");
        std::fs::write(&compiler_shim, b"soldr").expect("write compiler shim");

        assert_eq!(
            configured_daemon_executable(Some(canonical.clone().into_os_string())),
            Some(canonical)
        );
        assert!(
            configured_daemon_executable(Some(compiler_shim.into_os_string())).is_none(),
            "a compiler-named image must never be accepted as the daemon handoff"
        );
        assert!(configured_daemon_executable(None).is_none());
    });

    crate::timed_test!(only_the_main_soldr_image_is_safe_for_via_self_spawn, {
        assert!(executable_has_stem(
            Path::new(if cfg!(windows) {
                "C:\\tools\\soldr.exe"
            } else {
                "/opt/tools/soldr"
            }),
            "soldr"
        ));
        for unsafe_name in ["rustc", "clippy-driver", "zccache-soldr", "cargo"] {
            assert!(
                !executable_has_stem(Path::new(unsafe_name), "soldr"),
                "{unsafe_name} must not become a long-lived daemon image"
            );
        }
    });
}
