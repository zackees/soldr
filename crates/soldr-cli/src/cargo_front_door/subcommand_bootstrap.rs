fn suggest_cargo_subcommand_typo(sub: &str) -> Option<String> {
    if crate::cli_args::is_cargo_builtin_verb(sub) {
        return None;
    }
    let known = crate::fetch::known_cargo_subcommands();
    crate::fuzzy_match::suggest_close_match(sub, &known).map(|s| s.to_string())
}

/// Env var name for the PATH-first override (issue #816). Reads as truthy
/// when set to a recognised on-spelling (`1`/`true`/`yes`/`on`). An
/// unrecognised value does not force (soldr#2740).
pub(crate) const FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR: &str =
    "SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS";

fn force_managed_cargo_subcommands() -> bool {
    match std::env::var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR) {
        Ok(value) => {
            crate::core::flag_value(&value)
        }
        Err(_) => false,
    }
}

/// Walk `$PATH` looking for an executable named `tool`. Mirrors the
/// hand-rolled lookup in `core::toolchain_resolve::path_bin_dir` —
/// duplicated rather than re-exported to keep the cargo-front-door
/// independent of `core::toolchain_resolve`'s internals. On Windows the
/// `PATHEXT` suffix sweep matches what the toolchain resolver does so
/// `cargo-zigbuild.exe` is found even when the caller typed `cargo-zigbuild`.
fn find_on_path(tool: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(tool);
        if candidate.is_file() {
            return Some(candidate);
        }
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            if std::path::Path::new(tool).extension().is_some() {
                continue;
            }
            let pathext = std::env::var_os("PATHEXT")
                .and_then(|value| value.into_string().ok())
                .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
            for suffix in pathext.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                let suffixed = dir.join(format!("{tool}{suffix}"));
                if suffixed.is_file() {
                    return Some(suffixed);
                }
            }
        }
    }
    None
}

/// Result of subcommand tool resolution: PATH-prepended bin dirs +
/// env-var overrides for the child cargo invocation.
pub(crate) struct SubcommandToolBootstrap {
    pub bin_dirs: Vec<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
    pub cargo_args: Vec<String>,
}

pub(crate) async fn ensure_known_subcommand_tool(
    args: &[String],
    paths: &SoldrPaths,
) -> Result<SubcommandToolBootstrap, SoldrError> {
    let Some(sub) = first_cargo_subcommand(args) else {
        return Ok(SubcommandToolBootstrap {
            bin_dirs: Vec::new(),
            env: Vec::new(),
            cargo_args: Vec::new(),
        });
    };
    let Some(spec) = crate::fetch::lookup_by_cargo_subcommand(sub) else {
        // Issue #412: when the typed subcommand isn't in
        // `known_tools` but LOOKS like a typo of one that IS, drop a
        // "did you mean?" hint on stderr. We still return empty so the
        // underlying cargo invocation continues as today — the
        // suggestion is advisory and cargo's own external-command
        // dispatch may still find the tool on PATH.
        if let Some(suggestion) = suggest_cargo_subcommand_typo(sub) {
            eprintln!("soldr: '{sub}' is not a cargo subcommand soldr ships a prebuilt for.");
            eprintln!("soldr: did you mean: cargo {suggestion}?");
        }
        return Ok(SubcommandToolBootstrap {
            bin_dirs: Vec::new(),
            env: Vec::new(),
            cargo_args: Vec::new(),
        });
    };

    // Issue #816: if `cargo-<sub>` is already on PATH, defer to it instead
    // of running the managed fetch. This matches the discipline
    // `ensure_rustup_available` uses for rustup and avoids two failure modes:
    //   1. The managed fetcher writing an unrunnable artifact (the original
    //      #816 / #810 cargo-zigbuild bug, now fixed by xz2 extraction —
    //      but PATH-first is a structural belt-and-suspenders).
    //   2. Bypassing a user who deliberately installed a specific version
    //      via `cargo install <name>` or their distro package
    //      manager. cargo's own external-subcommand dispatch will find the
    //      PATH binary; soldr returning Ok(empty) here leaves that path
    //      open without prepending its own bin dir.
    // Escape hatch: SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS=1 forces the
    // managed fetch even when PATH has the tool — useful for CI runs that
    // want byte-identical pinned binaries.
    let mut extra_bin_dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut extra_env: Vec<(String, String)> = Vec::new();
    let mut extra_cargo_args: Vec<String> = Vec::new();

    if !force_managed_cargo_subcommands() {
        let exe_name = format!("cargo-{sub}");
        if let Some(path) = find_on_path(&exe_name) {
            if sub == "dylint" {
                let version = spec.pinned_version.unwrap_or("unknown");
                validate_dylint_path_binary(&path, "cargo-dylint", version)?;
                if let Some(link) = find_on_path("dylint-link") {
                    validate_dylint_path_binary(&link, "dylint-link", version)?;
                }
            }
            // Informational; on GitHub Actions it repeats once per nested
            // invocation (a Dylint cook runs dozens) and the override it
            // advertises is a workflow-level decision, not a per-line one.
            if !foreign_env_flag("GITHUB_ACTIONS") {
                eprintln!(
                    "soldr: deferring to {exe_name} on PATH at {} (set SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS=1 to override)",
                    path.display()
                );
            }
            // Even when cargo-zigbuild is provided by the host, it
            // still shells out to `zig`. Run the transitive bootstrap
            // before returning so the deferred-on-PATH branch doesn't
            // silently regress.
            append_subcommand_transitive_bin_dirs(
                sub,
                args,
                paths,
                &mut extra_bin_dirs,
                &mut extra_env,
                &mut extra_cargo_args,
            )
            .await?;
            return Ok(SubcommandToolBootstrap {
                bin_dirs: extra_bin_dirs,
                env: extra_env,
                cargo_args: extra_cargo_args,
            });
        }
    }

    let version = spec
        .pinned_version
        .map(|v| VersionSpec::Exact(v.to_string()))
        .unwrap_or(VersionSpec::Latest);

    eprintln!("soldr: fetching {}...", spec.crate_name);
    let result = match crate::fetch::fetch_tool_for_host_with_paths(
        spec.crate_name,
        &version,
        paths,
    )
    .await
    {
        Ok(result) => result,
        Err(error) if sub == "dylint" => {
            return Err(dylint_unavailable_error(
                "cargo-dylint",
                spec.pinned_version.unwrap_or("unknown"),
                &error,
            ));
        }
        Err(error) => return Err(error),
    };

    if result.cached {
        eprintln!(
            "soldr: using cached {} v{}",
            spec.crate_name, result.version
        );
    } else {
        eprintln!("soldr: downloaded {} v{}", spec.crate_name, result.version);
    }

    let dir = result
        .binary_path
        .parent()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "failed to resolve bin dir for fetched {}",
                spec.crate_name
            ))
        })?
        .to_path_buf();
    extra_bin_dirs.push(dir);
    append_subcommand_transitive_bin_dirs(
        sub,
        args,
        paths,
        &mut extra_bin_dirs,
        &mut extra_env,
        &mut extra_cargo_args,
    )
    .await?;
    Ok(SubcommandToolBootstrap {
        bin_dirs: extra_bin_dirs,
        env: extra_env,
        cargo_args: extra_cargo_args,
    })
}

async fn dylint_link_bin_dir(paths: &SoldrPaths) -> Result<std::path::PathBuf, SoldrError> {
    let pinned_version = crate::fetch::known_tools::lookup_by_crate("dylint-link")
        .and_then(|spec| spec.pinned_version)
        .ok_or_else(|| SoldrError::Other("dylint-link must have a registry pin".into()))?;
    let version = VersionSpec::Exact(pinned_version.to_string());
    eprintln!("soldr: fetching dylint-link...");
    match crate::fetch::fetch_tool_for_host_with_paths("dylint-link", &version, paths).await {
        Ok(result) => {
            if let Err(error) = validated_dylint_link_prebuilt(&result) {
                return Err(dylint_unavailable_error(
                    "dylint-link",
                    pinned_version,
                    &error,
                ));
            }
            if result.cached {
                eprintln!("soldr: using cached dylint-link v{}", result.version);
            } else {
                eprintln!("soldr: downloaded dylint-link v{}", result.version);
            }
            result
                .binary_path
                .parent()
                .map(std::path::Path::to_path_buf)
                .ok_or_else(|| {
                    SoldrError::Other(format!(
                        "failed to resolve bin dir for fetched dylint-link: {}",
                        result.binary_path.display()
                    ))
                })
        }
        Err(error) => Err(dylint_unavailable_error(
            "dylint-link",
            pinned_version,
            &error,
        )),
    }
}

fn validated_dylint_link_prebuilt(result: &crate::fetch::FetchResult) -> Result<(), SoldrError> {
    let target = crate::core::TargetTriple::host()?;
    crate::fetch::smoke_test_or_evict(&result.binary_path, "dylint-link", &target)
}

fn dylint_unavailable_error(component: &str, version: &str, error: &SoldrError) -> SoldrError {
    let host = crate::core::TargetTriple::host()
        .map(|target| target.triple().to_string())
        .unwrap_or_else(|_| "unknown-host".to_string());
    SoldrError::Other(format!(
        "Dylint v{version} is not built for this machine (host: {host}; missing or unusable \
         component: {component}). Soldr will not build Dylint from source. Cause: {error}. \
         Corrective action: install compatible Dylint v{version} binaries for {host} on PATH \
         (and dylint-driver under DYLINT_DRIVER_PATH), publish matching prebuilt release assets, \
         or select a Dylint version that provides {host} prebuilts."
    ))
}

fn validate_dylint_path_binary(
    binary: &Path,
    component: &str,
    version: &str,
) -> Result<(), SoldrError> {
    let host = crate::core::TargetTriple::host()?;
    let mut failures = Vec::new();
    for argument in ["--version", "--help"] {
        let mut command = std::process::Command::new(binary);
        command
            .arg(argument)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if component == "dylint-link" {
            command.env("RUSTUP_TOOLCHAIN", format!("nightly-{}", host.triple()));
        }
        suppress_windows_console_window(&mut command);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                failures.push(format!("{argument} could not start: {error}"));
                continue;
            }
        };
        match child.wait_timeout(Duration::from_secs(2)) {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => failures.push(format!("{argument} exited with {status}")),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                failures.push(format!("{argument} exceeded 2 seconds"));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                failures.push(format!("{argument} wait failed: {error}"));
            }
        }
    }
    Err(dylint_unavailable_error(
        component,
        version,
        &SoldrError::Other(format!(
            "PATH binary at {} failed bounded validation: {}",
            binary.display(),
            failures.join("; ")
        )),
    ))
}

// Pick the cargo-dylint binary to use given the outcome of the managed
// prebuilt fetch. Source compilation is intentionally never consulted.
//
// Split out of `ensure_known_subcommand_tool` so the binary-only policy is
// unit-testable without a network round-trip: the fetch outcome is an
// already-resolved `Result` and the forbidden source build is a closure.
//
// The failure this exists for is a *smoke-test* failure, not a download
// failure. `fetch_tool_for_host_with_paths` runs `--version` on the
// extracted binary (soldr#936, `smoke_test_or_evict`) and evicts it on a
// non-zero exit, so an incompatible upstream Dylint asset download
// fine on Debian 12 and then fails the probe with a loader error — which
// is exactly the `Err` arm below.
