//! Docker-delegated Linux cross builds (soldr#2319, Approach C).
//!
//! Building `*-unknown-linux-gnu` from a Windows host cannot use the catalogue
//! `gnu-linux-toolchain` bundle directly: that bundle ships a Linux-ELF
//! `gcc`/`ld`, which a Windows host cannot exec (`os error 193`). Rather than
//! building a Windows-hosted Canadian-cross GCC (Approach A) or a Win-clang +
//! GNU-sysroot hybrid (Approach B), Approach C delegates the whole compile to a
//! Linux Docker container. Inside Linux, soldr's existing blessed linux-gnu
//! path uses the catalogue **gcc-13.3.0-glibc-2.17** toolchain and produces a
//! glibc-2.17 ELF -- byte-for-byte the same story a Linux release lane tells,
//! because it *is* a Linux build.
//!
//! The dispatch decision (`should_delegate_to_docker`) and the `docker run`
//! argv builder (`docker_command`) are pure and unit-tested on every CI host,
//! so the Windows-only behavior is proven on Linux CI without Docker (CLAUDE.md
//! Agent-Dev rule). `run` adds the impure parts: the `docker version` probe
//! (with an actionable absent-Docker error, item C2) and the streamed exec.

use crate::core::SoldrError;

/// Container base image. `python:3.12-slim-bookworm` is Debian 12 (glibc 2.36
/// host libc), but the produced *target* ELF floors at glibc 2.17 because the
/// container's soldr links against the catalogue `gnu-linux-toolchain` sysroot,
/// not the container's own libc. It also ships `pip`, so bootstrapping soldr is
/// one line.
pub(crate) const CONTAINER_IMAGE: &str = "python:3.12-slim-bookworm";

/// Named docker volume backing the container's `~/.cargo` (registry + git deps)
/// so repeat cross builds are warm.
pub(crate) const CARGO_VOLUME: &str = "soldr-cross-cargo";

/// Named docker volume backing the container's `~/.soldr` (catalogue toolchain
/// + daemon cache) so the glibc-2.17 bundle is fetched once and reused.
pub(crate) const SOLDR_VOLUME: &str = "soldr-cross-soldr";

/// Escape hatch: `SOLDR_NO_DOCKER_CROSS=1` disables delegation entirely, so the
/// build falls through to the normal (failing, on Windows) native path -- kept
/// so the raw failure is still reachable for diagnostics.
pub(crate) const DISABLE_ENV: &str = "SOLDR_NO_DOCKER_CROSS";

/// The host OS, as an injectable seam so `should_delegate_to_docker` is
/// testable on Linux CI. `Host::current()` resolves to the real host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Host {
    Windows,
    Other,
}

impl Host {
    pub(crate) fn current() -> Self {
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            Host::Windows
        } else {
            Host::Other
        }
    }
}

/// The subset of process env the dispatch decision reads, snapshotted so the
/// decision function stays pure/testable.
#[derive(Debug, Clone, Default)]
pub(crate) struct EnvSnapshot {
    pub(crate) disabled: bool,
}

/// Does this `SOLDR_NO_DOCKER_CROSS` value mean "disabled"?
///
/// Pure so the test exercises *this* function rather than a copy of it
/// (soldr#2740). The previous test re-implemented the expression inline, so
/// it validated a duplicate and could not catch a drift between the two.
///
/// Denylist, not allowlist: anything that is not a recognised falsy spelling
/// disables. The falsy set is the full one -- the earlier version omitted
/// `no` and `off`, so `SOLDR_NO_DOCKER_CROSS=off` disabled Docker delegation,
/// the exact opposite of what someone writing `off` means.
pub(crate) fn disable_value_means_disabled(raw: &str) -> bool {
    crate::core::flag_value(raw)
}

impl EnvSnapshot {
    pub(crate) fn from_process() -> Self {
        let disabled = std::env::var(DISABLE_ENV)
            .map(|v| disable_value_means_disabled(&v))
            .unwrap_or(false);
        EnvSnapshot { disabled }
    }
}

/// True when a `soldr build --target <target>` should be delegated to a Linux
/// Docker container: Windows host, a `*-unknown-linux-gnu` target, and the
/// escape hatch not set. Every other combination (non-Windows host, musl /
/// windows / darwin targets, escape hatch) returns false and is completely
/// unaffected.
pub(crate) fn should_delegate_to_docker(host: Host, target: &str, env: &EnvSnapshot) -> bool {
    if env.disabled {
        return false;
    }
    if host != Host::Windows {
        return false;
    }
    target.ends_with("-unknown-linux-gnu")
}

/// The version of soldr to install inside the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContainerSoldrVersion {
    /// `pip install soldr==X.Y.Z` -- pinned to the host soldr's own version so
    /// host and container agree.
    Pinned(String),
    /// `pip install soldr` -- latest; used when the host's exact version is not
    /// yet on PyPI (dev builds).
    Latest,
}

impl ContainerSoldrVersion {
    /// The `soldr[==X.Y.Z]` argument for `pip install`.
    fn pip_spec(&self) -> String {
        match self {
            ContainerSoldrVersion::Pinned(v) => format!("soldr=={v}"),
            ContainerSoldrVersion::Latest => "soldr".to_string(),
        }
    }
}

/// Build the `docker run` argv (pure -- unit-tested).
///
/// `project_dir` is the host path mounted at `/work`; on Windows the caller
/// normalizes backslashes to forward slashes first so Docker Desktop's
/// drive-letter parsing accepts it. `passthrough_args` are the user's build
/// args with the leading `build` verb and any `--target`/`--target=` flag
/// stripped -- `--target <target>` is re-added here so the container invocation
/// is canonical regardless of how the user spelled it.
pub(crate) fn docker_command(
    project_dir: &str,
    target: &str,
    soldr_version: &ContainerSoldrVersion,
    passthrough_args: &[String],
) -> Vec<String> {
    // The container is a fresh Linux env: pip-install soldr, then
    // `soldr toolchain install` so rustup + the project's pinned channel
    // (rust-toolchain.toml) exist before the build — without it the fresh
    // rustup has no default toolchain and `rustup target add` fails. The
    // /root/.soldr volume persists the toolchain, so this is a one-time cost.
    let mut inner = format!(
        "pip install --quiet {} && soldr toolchain install && \
         soldr build --target {}",
        soldr_version.pip_spec(),
        target
    );
    for arg in passthrough_args {
        inner.push(' ');
        inner.push_str(&shell_quote(arg));
    }

    vec![
        "run".to_string(),
        "--rm".to_string(),
        "-v".to_string(),
        format!("{project_dir}:/work"),
        "-w".to_string(),
        "/work".to_string(),
        "-v".to_string(),
        format!("{CARGO_VOLUME}:/root/.cargo"),
        "-v".to_string(),
        format!("{SOLDR_VOLUME}:/root/.soldr"),
        CONTAINER_IMAGE.to_string(),
        "bash".to_string(),
        "-lc".to_string(),
        inner,
    ]
}

/// Minimal POSIX single-quote quoting for the args interpolated into the
/// container's `bash -lc` string.
fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b'='))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Strip the leading `build` verb and any `--target`/`--target=` flag from the
/// user's build args, leaving the passthrough the container invocation appends
/// after `--target <target>`.
fn passthrough_from_full_args(full_args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(full_args.len());
    let mut skip_next = false;
    for (i, arg) in full_args.iter().enumerate() {
        if i == 0 && arg == "build" {
            continue;
        }
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--target" {
            skip_next = true;
            continue;
        }
        if arg.starts_with("--target=") {
            continue;
        }
        out.push(arg.clone());
    }
    out
}

/// Resolve which soldr version to install in the container: pin to the host's
/// own version when it is published on PyPI, else fall back to latest and log.
async fn resolve_container_version() -> ContainerSoldrVersion {
    let host_version = env!("CARGO_PKG_VERSION");
    if pypi_has_version("soldr", host_version).await {
        ContainerSoldrVersion::Pinned(host_version.to_string())
    } else {
        eprintln!(
            "soldr: version {host_version} is not on PyPI yet (dev build); \
             installing the latest published soldr in the Linux cross container"
        );
        ContainerSoldrVersion::Latest
    }
}

/// True when `https://pypi.org/pypi/<pkg>/<version>/json` returns 200. Any
/// network error is treated as "not available" so a transient failure degrades
/// to the latest-tag fallback rather than aborting the build.
async fn pypi_has_version(pkg: &str, version: &str) -> bool {
    crate::fetch::pypi_has_version(pkg, version).await
}

/// Probe `docker version`. Returns an actionable error (item C2) when Docker is
/// absent or the engine is not running, replacing the late `os error 193`.
async fn ensure_docker_available() -> Result<(), SoldrError> {
    let probe = tokio::process::Command::new("docker")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    match probe {
        Ok(status) if status.success() => Ok(()),
        _ => Err(SoldrError::Other(docker_missing_message())),
    }
}

fn docker_missing_message() -> String {
    "building *-unknown-linux-gnu from a Windows host requires Docker Desktop \
     -- soldr compiles the Linux target inside a Linux container (soldr#2319). \
     Install/start Docker, or build on a Linux host. \
     (Set SOLDR_NO_DOCKER_CROSS=1 to bypass this and use the native path.)"
        .to_string()
}

/// Normalize a host project path for a Docker `-v` bind mount. On Windows,
/// backslashes are converted to forward slashes so Docker Desktop's
/// drive-letter (`C:/...`) parsing accepts the source path.
fn normalize_mount_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// Delegate `soldr build --target <target> ...` to a Linux Docker container.
/// Probes Docker first (actionable error when absent), then execs `docker run`
/// with inherited stdio and propagates the container's exit code.
pub(crate) async fn run(target: &str, full_args: &[String]) -> Result<i32, SoldrError> {
    ensure_docker_available().await?;

    let project_dir = std::env::current_dir()
        .map_err(|e| SoldrError::Other(format!("cannot resolve current directory: {e}")))?;
    let project_dir = normalize_mount_path(&project_dir.to_string_lossy());

    let version = resolve_container_version().await;
    let passthrough = passthrough_from_full_args(full_args);
    let argv = docker_command(&project_dir, target, &version, &passthrough);

    eprintln!(
        "soldr: delegating {target} build to Docker Linux (soldr#2319): docker {}",
        argv.join(" ")
    );

    // soldr#2718: `status()` hands this process's stdio to `docker run`, so
    // every diagnostic the container's soldr writes -- compile errors, a
    // missing rust-toolchain.toml, an unresolved dependency -- reaches the
    // user through our streams. That is exactly the "spawns a child that
    // inherits stdio" case `exit_guard` asks callers to record. Without it
    // the delegation site's `guarded_exit` sees `spoke() == false` and
    // appends "soldr emitted no diagnostic and ran no child process ...
    // this is a fault in soldr itself", directly under the container's
    // perfectly good explanation. Marked before the spawn, not after, so it
    // holds no matter how the child fares or which exit path runs next.
    crate::exit_guard::mark_spoke();
    let status = tokio::process::Command::new("docker")
        .args(&argv)
        .status()
        .await
        .map_err(|e| SoldrError::Other(format!("failed to launch docker: {e}")))?;

    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(disabled: bool) -> EnvSnapshot {
        EnvSnapshot { disabled }
    }

    #[test]
    fn delegates_on_windows_linux_gnu() {
        assert!(should_delegate_to_docker(
            Host::Windows,
            "x86_64-unknown-linux-gnu",
            &env(false)
        ));
        assert!(should_delegate_to_docker(
            Host::Windows,
            "aarch64-unknown-linux-gnu",
            &env(false)
        ));
    }

    #[test]
    fn no_delegate_for_non_gnu_targets() {
        for target in [
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        ] {
            assert!(
                !should_delegate_to_docker(Host::Windows, target, &env(false)),
                "unexpected delegation for {target}"
            );
        }
    }

    #[test]
    fn no_delegate_on_non_windows_host() {
        assert!(!should_delegate_to_docker(
            Host::Other,
            "x86_64-unknown-linux-gnu",
            &env(false)
        ));
    }

    #[test]
    fn escape_hatch_disables_delegation() {
        assert!(!should_delegate_to_docker(
            Host::Windows,
            "x86_64-unknown-linux-gnu",
            &env(true)
        ));
    }

    /// soldr#2740: exercises the real parser, not a copy of it, and covers
    /// the two spellings that used to invert.
    #[test]
    fn env_snapshot_parses_disable_values() {
        for (raw, want) in [
            ("1", true),
            ("true", true),
            ("yes", true),
            // soldr#2740: an owned switch takes the allowlist rule, so an
            // unrecognised value no longer disables delegation.
            ("enabled", false),
            ("maybe", false),
            ("0", false),
            ("false", false),
            ("FALSE", false),
            // These two disabled Docker delegation before soldr#2740 --
            // the opposite of what someone writing them means.
            ("no", false),
            ("off", false),
            ("OFF", false),
            (" off ", false),
            ("", false),
        ] {
            assert_eq!(
                disable_value_means_disabled(raw),
                want,
                "disable parse for {raw:?}"
            );
        }
    }

    #[test]
    fn docker_command_argv_snapshot() {
        let argv = docker_command(
            "/home/u/proj",
            "x86_64-unknown-linux-gnu",
            &ContainerSoldrVersion::Pinned("0.8.42".to_string()),
            &[
                "--release".to_string(),
                "-p".to_string(),
                "demo".to_string(),
            ],
        );
        let expected = vec![
            "run",
            "--rm",
            "-v",
            "/home/u/proj:/work",
            "-w",
            "/work",
            "-v",
            "soldr-cross-cargo:/root/.cargo",
            "-v",
            "soldr-cross-soldr:/root/.soldr",
            "python:3.12-slim-bookworm",
            "bash",
            "-lc",
            "pip install --quiet soldr==0.8.42 && soldr toolchain install && \
             soldr build --target x86_64-unknown-linux-gnu --release -p demo",
        ];
        assert_eq!(argv, expected);
    }

    #[test]
    fn docker_command_latest_fallback() {
        let argv = docker_command(
            "/w",
            "aarch64-unknown-linux-gnu",
            &ContainerSoldrVersion::Latest,
            &[],
        );
        // Latest => unpinned `soldr`, and --target is always present.
        let inner = argv.last().unwrap();
        assert_eq!(
            inner,
            "pip install --quiet soldr && soldr toolchain install && \
             soldr build --target aarch64-unknown-linux-gnu"
        );
        assert!(argv.contains(&"python:3.12-slim-bookworm".to_string()));
    }

    #[test]
    fn passthrough_strips_verb_and_target() {
        let full = vec![
            "build".to_string(),
            "--target".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
            "--release".to_string(),
            "-p".to_string(),
            "demo".to_string(),
        ];
        assert_eq!(
            passthrough_from_full_args(&full),
            vec![
                "--release".to_string(),
                "-p".to_string(),
                "demo".to_string()
            ]
        );

        // `--target=` spelling is also stripped.
        let eq = vec![
            "build".to_string(),
            "--target=aarch64-unknown-linux-gnu".to_string(),
            "--verbose".to_string(),
        ];
        assert_eq!(
            passthrough_from_full_args(&eq),
            vec!["--verbose".to_string()]
        );
    }

    #[test]
    fn mount_path_normalizes_backslashes() {
        assert_eq!(
            normalize_mount_path("C:\\Users\\niteris\\dev\\soldr2"),
            "C:/Users/niteris/dev/soldr2"
        );
        assert_eq!(normalize_mount_path("/home/u/p"), "/home/u/p");
    }

    #[test]
    fn docker_missing_message_is_actionable() {
        let msg = docker_missing_message();
        assert!(msg.contains("Docker"));
        assert!(msg.contains("soldr#2319"));
        assert!(msg.contains("Linux host"));
        // The old failure mode must not be what the user sees.
        assert!(!msg.contains("os error 193"));
    }
}
