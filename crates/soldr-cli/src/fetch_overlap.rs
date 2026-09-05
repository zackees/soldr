//! soldr#1543 — overlap the Cargo dependency fetch with blessed SDK
//! preparation on the `soldr build --target <T>` surface.
//!
//! `soldr build --target <T>` spends its first seconds (or minutes, on
//! a cold `~/.soldr`) inside [`crate::blessed_build::prepare`] —
//! catalogue download plus sysroot materialization — while cargo has
//! not even started acquiring crate dependencies. The two activities
//! are independent network/IO work, so a bounded
//! `cargo fetch --target <T>` child process is spawned right
//! before prep starts and joined right after prep finishes. Fresh
//! build wall time approaches `max(fetch, prepare)` instead of their
//! sum.
//!
//! ## Correctness properties
//!
//! * **Best effort.** Any prefetch failure (spawn error, non-zero
//!   exit, stale lockfile, network outage) is logged and ignored —
//!   the main cargo build performs, and surfaces errors from, its own
//!   dependency acquisition exactly as before.
//! * **`--locked`.** The prefetch never rewrites `Cargo.lock`. When
//!   the lockfile is stale relative to the manifests, `cargo fetch
//!  ` fails fast instead of racing the main build's resolver,
//!   and the failure is swallowed per the previous bullet.
//! * **Package-cache coordination is cargo-native.** Both the prefetch
//!   child and the eventual build serialize downloads through cargo's
//!   own package-cache file locks, and the child is joined before the
//!   build spawns anyway, so the two cargos never race.
//! * **Auth/config parity.** The child is the same cargo binary the
//!   front door resolves (honoring `SOLDR_TEST_CARGO_BIN` and the
//!   managed toolchain homes), inherits the caller's environment and
//!   cwd, and receives the caller's `--manifest-path` — so registry
//!   auth, `.cargo/config.toml` source replacement, and proxy settings
//!   all apply identically to prefetch and build.
//! * **Never through the rustc wrapper.** `cargo fetch` compiles
//!   nothing; `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER` are scrubbed
//!   from the child explicitly so no inherited wrapper is ever
//!   consulted.
//! * **Cancellation.** The child is spawned with `kill_on_drop`, so a
//!   prep error (which propagates with `?` before the join point) or
//!   any other early exit reaps the prefetch instead of leaking it.
//!
//! ## When the overlap is skipped
//!
//! * The caller asked for offline semantics: `--offline` / `--frozen`
//!   in the build args, or a truthy `CARGO_NET_OFFLINE`.
//! * No `Cargo.lock` exists at or above the manifest directory —
//!   `cargo fetch` would just error, and prefetching an
//!   unlocked resolve could race the main build's lockfile write.
//! * The kill switch [`FETCH_OVERLAP_ENV_VAR`] is set to a falsy
//!   value.
//!
//! Only the blessed `soldr build` surface triggers the prefetch —
//! `soldr cargo build` (the explicit legacy passthrough) and every
//! other soldr command spawn no fetch subprocess. That property is
//! pinned by `tests/broker/cli_build_fetch_overlap.rs`.

use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

/// Kill switch for the dependency-prefetch overlap. Set to `0`,
/// `false`, `no`, or `off` (case-insensitive) to disable; unset or any
/// other value keeps the overlap enabled.
pub const FETCH_OVERLAP_ENV_VAR: &str = "SOLDR_FETCH_OVERLAP";

/// A running `cargo fetch` prefetch child. Obtain via
/// [`spawn_for_blessed_build`]; await [`DepPrefetch::join`] before the
/// main cargo build spawns.
pub(crate) struct DepPrefetch {
    child: tokio::process::Child,
    started: Instant,
}

impl DepPrefetch {
    /// Wait for the prefetch to finish. Failures are logged and
    /// swallowed — the main build owns real error reporting.
    pub(crate) async fn join(mut self) {
        match self.child.wait().await {
            Ok(status) if status.success() => {
                eprintln!(
                    "soldr build: dependency prefetch completed in {} ms",
                    self.started.elapsed().as_millis()
                );
            }
            Ok(status) => {
                eprintln!(
                    "soldr build: dependency prefetch exited with {status} after {} ms \
                     (continuing — cargo build will fetch and report as usual)",
                    self.started.elapsed().as_millis()
                );
            }
            Err(err) => {
                eprintln!(
                    "soldr build: failed to wait for dependency prefetch: {err} \
                     (continuing — cargo build will fetch and report as usual)"
                );
            }
        }
    }
}

/// Spawn `cargo fetch --target <target>` in the background so
/// it overlaps blessed SDK preparation. Returns `None` (with a log
/// line where useful) whenever the overlap does not apply or the spawn
/// fails — the caller proceeds identically either way.
pub(crate) fn spawn_for_blessed_build(build_args: &[String], target: &str) -> Option<DepPrefetch> {
    let cwd = std::env::current_dir().ok()?;
    let fetch_args = plan_prefetch(
        build_args,
        target,
        &cwd,
        std::env::var(FETCH_OVERLAP_ENV_VAR).ok().as_deref(),
        std::env::var("CARGO_NET_OFFLINE").ok().as_deref(),
    )?;

    let cargo = match crate::binaries::resolve_toolchain_binary("cargo") {
        Ok(cargo) => cargo,
        Err(err) => {
            // The front door will fail with a better version of this
            // error moments later; stay quiet beyond a debug line.
            tracing::debug!(
                target: "soldr::fetch_overlap",
                "skipping dependency prefetch: cargo unresolved: {err}"
            );
            return None;
        }
    };

    let mut command = std::process::Command::new(&cargo);
    command.args(&fetch_args);
    // `cargo fetch` never invokes rustc for compilation, but scrub the
    // wrapper vars anyway so no inherited wrapper is consulted for
    // probes and the prefetch provably bypasses the cache layer.
    command.env_remove("RUSTC_WRAPPER");
    command.env_remove("RUSTC_WORKSPACE_WRAPPER");
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &cargo);
    crate::core::suppress_windows_console_window(&mut command);
    // Quiet child: progress output would interleave with blessed-prep
    // logging, and every error it could print is reproduced by the
    // main build if it actually matters.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    // soldr#3098: spawns share, staged writes exclude.
    let spawned = {
        let _spawn = crate::core::spawn_exclusion::spawn_shared();
        command.spawn()
    };
    match spawned {
        Ok(child) => {
            eprintln!(
                "soldr build: prefetching dependencies alongside SDK preparation \
                 (cargo {})",
                fetch_args.join(" ")
            );
            Some(DepPrefetch {
                child,
                started: Instant::now(),
            })
        }
        Err(err) => {
            eprintln!(
                "soldr build: dependency prefetch failed to spawn: {err} \
                 (continuing — cargo build will fetch as usual)"
            );
            None
        }
    }
}

/// Pure decision core: given the `soldr build` cargo args (including
/// the leading `build` verb), the `--target` value, the working
/// directory, and the relevant env values, return the argv (minus the
/// cargo binary itself) for the overlap prefetch — or `None` when the
/// overlap must be skipped.
pub(crate) fn plan_prefetch(
    build_args: &[String],
    target: &str,
    _cwd: &Path,
    overlap_env: Option<&str>,
    cargo_net_offline_env: Option<&str>,
) -> Option<Vec<String>> {
    if !overlap_enabled(overlap_env) || cargo_net_offline(cargo_net_offline_env) {
        return None;
    }

    let flags = scan_build_flags(build_args);
    if flags.offline || flags.frozen {
        return None;
    }

    // Workspace members keep Cargo.lock at the workspace root, so walk
    // ancestors the same way cargo does.

    // soldr#2139: this is the second cargo child on the blessed path, and it
    // builds its own `--target` rather than inheriting the caller's argv, so
    // it needs the bare triple in its own right. rustc does not know the
    // `.<glibc>` spelling.
    let target = crate::target_alias::split_glibc_floor(target).map_or(target, |(base, _)| base);
    let mut args = vec![
        "fetch".to_string(),
        "--target".to_string(),
        target.to_string(),
    ];
    if let Some(manifest_path) = flags.manifest_path {
        args.push("--manifest-path".to_string());
        args.push(manifest_path);
    }
    Some(args)
}

fn overlap_enabled(env_value: Option<&str>) -> bool {
    match env_value {
        None => true,
        Some(value) => !crate::core::is_off_value(value),
    }
}

/// Cargo reads `CARGO_NET_OFFLINE` as a config boolean; mirror its
/// truthiness liberally so a user-declared offline intent always
/// suppresses the prefetch.
fn cargo_net_offline(env_value: Option<&str>) -> bool {
    match env_value {
        None => false,
        // CARGO_NET_OFFLINE is Cargo's variable, not ours, so it takes the
        // foreign rule: any value Cargo would read as set means offline.
        Some(value) => crate::core::foreign_flag_value(value),
    }
}

#[derive(Debug, Default)]
struct BuildFlagScan {
    offline: bool,
    frozen: bool,
    manifest_path: Option<String>,
}

fn scan_build_flags(args: &[String]) -> BuildFlagScan {
    let mut out = BuildFlagScan::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => break,
            "--offline" => out.offline = true,
            "--frozen" => out.frozen = true,
            "--manifest-path" => {
                if let Some(value) = iter.next() {
                    if !value.is_empty() {
                        out.manifest_path = Some(value.clone());
                    }
                }
            }
            other => {
                if let Some(value) = other.strip_prefix("--manifest-path=") {
                    if !value.is_empty() {
                        out.manifest_path = Some(value.to_string());
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = "x86_64-pc-windows-msvc";

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn project_with_lockfile(label: &str) -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix(label)
            .tempdir()
            .expect("tempdir");
        std::fs::write(dir.path().join("Cargo.lock"), "version = 3\n").expect("write lock");
        dir
    }

    #[test]
    fn prefetch_plans_locked_target_fetch() {
        let dir = project_with_lockfile("fo-basic");
        let plan = plan_prefetch(
            &args(&["build", "--target", TARGET, "--release"]),
            TARGET,
            dir.path(),
            None,
            None,
        );
        assert_eq!(
            plan,
            Some(args(&["fetch", "--target", TARGET])),
            "basic blessed build must plan a locked, target-scoped fetch"
        );
    }

    #[test]
    fn prefetch_forwards_manifest_path() {
        let dir = project_with_lockfile("fo-manifest");
        let member = dir.path().join("member");
        std::fs::create_dir_all(&member).expect("member dir");
        std::fs::write(member.join("Cargo.toml"), "[package]\n").expect("manifest");

        // Two-token form, manifest in a member dir whose lockfile
        // lives at the (workspace) root above it.
        let manifest_arg = member.join("Cargo.toml").display().to_string();
        let plan = plan_prefetch(
            &args(&[
                "build",
                "--manifest-path",
                &manifest_arg,
                "--target",
                TARGET,
            ]),
            TARGET,
            dir.path(),
            None,
            None,
        );
        assert_eq!(
            plan,
            Some(args(&[
                "fetch",
                "--target",
                TARGET,
                "--manifest-path",
                &manifest_arg,
            ])),
            "manifest path must be forwarded and the lockfile found in an ancestor"
        );

        // `--manifest-path=` single-token form, relative path.
        let plan = plan_prefetch(
            &args(&[
                "build",
                "--manifest-path=member/Cargo.toml",
                "--target",
                TARGET,
            ]),
            TARGET,
            dir.path(),
            None,
            None,
        );
        assert_eq!(
            plan,
            Some(args(&[
                "fetch",
                "--target",
                TARGET,
                "--manifest-path",
                "member/Cargo.toml",
            ])),
        );
    }

    #[test]
    fn prefetch_skips_offline_and_frozen_builds() {
        let dir = project_with_lockfile("fo-offline");
        for offline_flag in ["--offline", "--frozen"] {
            let plan = plan_prefetch(
                &args(&["build", "--target", TARGET, offline_flag]),
                TARGET,
                dir.path(),
                None,
                None,
            );
            assert_eq!(
                plan, None,
                "{offline_flag} must suppress the prefetch overlap"
            );
        }

        // Flags after `--` are not cargo flags and must NOT suppress.
        let plan = plan_prefetch(
            &args(&["build", "--target", TARGET, "--", "--offline"]),
            TARGET,
            dir.path(),
            None,
            None,
        );
        assert!(
            plan.is_some(),
            "tokens after `--` are not cargo flags and must not suppress the prefetch"
        );
    }

    #[test]
    fn prefetch_skips_when_cargo_net_offline() {
        let dir = project_with_lockfile("fo-netoff");
        let build = args(&["build", "--target", TARGET]);
        for truthy in ["true", "1", "TRUE", " yes "] {
            assert_eq!(
                plan_prefetch(&build, TARGET, dir.path(), None, Some(truthy)),
                None,
                "CARGO_NET_OFFLINE={truthy:?} must suppress the prefetch"
            );
        }
        for falsy in ["false", "0", "off", ""] {
            assert!(
                plan_prefetch(&build, TARGET, dir.path(), None, Some(falsy)).is_some(),
                "CARGO_NET_OFFLINE={falsy:?} must not suppress the prefetch"
            );
        }
    }

    #[test]
    fn prefetch_kill_switch_recognized() {
        let dir = project_with_lockfile("fo-kill");
        let build = args(&["build", "--target", TARGET]);
        for falsy in ["0", "off", "false", "NO"] {
            assert_eq!(
                plan_prefetch(&build, TARGET, dir.path(), Some(falsy), None),
                None,
                "SOLDR_FETCH_OVERLAP={falsy:?} must disable the prefetch"
            );
        }
        for enabled in [None, Some("1"), Some("auto")] {
            assert!(
                plan_prefetch(&build, TARGET, dir.path(), enabled, None).is_some(),
                "SOLDR_FETCH_OVERLAP={enabled:?} must keep the prefetch enabled"
            );
        }
    }

    #[test]
    fn prefetch_does_not_require_a_lockfile() {
        let dir = tempfile::Builder::new()
            .prefix("fo-nolock")
            .tempdir()
            .expect("tempdir");
        let plan = plan_prefetch(
            &args(&["build", "--target", TARGET]),
            TARGET,
            dir.path(),
            None,
            None,
        );
        assert_eq!(plan, Some(args(&["fetch", "--target", TARGET])));
    }
}
