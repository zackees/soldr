//! Bootstrap `rustup` from `https://static.rust-lang.org/rustup/dist/` when it
//! is missing on the host. Rust port of the logic that previously lived only in
//! `.github/actions/setup-soldr/ensure_rust_toolchain.py` (issue #406).
//!
//! Goal: turn the soldr CLI binary into a true one-stop entry point. On any
//! environment without a system-managed toolchain manager (e.g. nektos/act with
//! the `catthehacker/ubuntu:act-24.04` image, fresh container shells, alternative
//! CI providers), calling a soldr subcommand that needs `rustup` now triggers a
//! transparent install instead of failing with `command not found`.
//!
//! Two surfaces:
//!
//! - **Explicit:** [`bootstrap_rustup`] is what `soldr bootstrap` calls.
//! - **Implicit:** [`auto_bootstrap_if_missing_blocking`] runs from
//!   `crates/soldr-cli/src/binaries.rs` right before any rustup invocation when
//!   the binary is not on PATH and the user has not opted out via
//!   `SOLDR_NO_BOOTSTRAP=1`.
//!
//! Output of either path: a working `rustup` binary inside the soldr-managed
//! `bin/` directory. No global PATH mutation. The caller is responsible for
//! exporting the soldr-managed `CARGO_HOME` / `RUSTUP_HOME` to child processes
//! (`apply_implicit_toolchain_homes` already does this).
//!
//! The rustup-init binary is downloaded with the same trust policy as every
//! other soldr-fetched binary: pin via `SOLDR_CHECKSUMS_FILE`,
//! `SOLDR_TRUST_MODE=strict` refuses unpinned fetches.

use super::http_client;
use super::trust::{sha256_of, verify_download, PinnedChecksumStore, TrustMode, VerifyOutcome};
use crate::core::{SoldrError, SoldrPaths};
use std::path::{Path, PathBuf};

/// Opt-out env var. When set to a truthy value (`1`, `true`, `yes`, `on`,
/// case-insensitive), the auto-bootstrap path becomes a no-op and the caller
/// falls back to the legacy "rustup not found" diagnostic.
pub const NO_BOOTSTRAP_ENV_VAR: &str = "SOLDR_NO_BOOTSTRAP";

/// Override env var for the rustup-init host triple. Useful for tests that
/// exercise per-platform URL construction without depending on the runner's
/// host.
pub const RUSTUP_INIT_TRIPLE_ENV_VAR: &str = "SOLDR_RUSTUP_INIT_TRIPLE_OVERRIDE";

/// Override env var for the rustup-init download URL. Lets internal CI image
/// tests point at a fixture server.
pub const RUSTUP_INIT_URL_ENV_VAR: &str = "SOLDR_RUSTUP_INIT_URL_OVERRIDE";

const RUSTUP_INIT_TOOL_NAME: &str = "rustup-init";
const RUSTUP_INIT_PSEUDO_VERSION: &str = "latest";

/// Result of a bootstrap attempt.
#[derive(Debug, Clone)]
pub struct BootstrapReport {
    /// Resolved path to the installed `rustup` binary (inside the soldr-managed
    /// bin directory).
    pub rustup_path: PathBuf,
    /// True if `rustup` was already present (nothing was downloaded).
    pub already_installed: bool,
    /// URL the rustup-init binary was fetched from, when applicable.
    pub source_url: Option<String>,
}

/// Outcome of an auto-bootstrap attempt from a sync caller.
#[derive(Debug)]
pub enum AutoBootstrapOutcome {
    /// `rustup` was already discoverable (PATH or soldr-managed bin).
    AlreadyInstalled(PathBuf),
    /// Bootstrap ran and installed rustup.
    Installed(BootstrapReport),
    /// User opted out via [`NO_BOOTSTRAP_ENV_VAR`]. Caller should fall back to
    /// the legacy missing-rustup diagnostic.
    OptedOut,
}

/// Look up `rustup` on PATH or inside the soldr-managed bin dir.
///
/// This is the cheap discovery that runs on every CLI invocation. Returns
/// `None` only if the host has no rustup at all.
pub fn discover_rustup(paths: &SoldrPaths) -> Option<PathBuf> {
    if let Some(p) = which_on_path("rustup") {
        return Some(p);
    }
    let managed = managed_rustup_path(paths);
    if managed.is_file() {
        return Some(managed);
    }
    None
}

/// Sync wrapper around the async bootstrap. Spawns a short-lived Tokio current-
/// thread runtime on a dedicated OS thread so the call works whether the caller
/// is inside an existing runtime (`#[tokio::main]`) or not.
pub fn auto_bootstrap_if_missing_blocking(
    paths: &SoldrPaths,
) -> Result<AutoBootstrapOutcome, SoldrError> {
    if let Some(p) = discover_rustup(paths) {
        return Ok(AutoBootstrapOutcome::AlreadyInstalled(p));
    }
    if no_bootstrap_opt_out() {
        return Ok(AutoBootstrapOutcome::OptedOut);
    }
    eprintln!(
        "soldr: rustup not found; bootstrapping into {} \
         (set {NO_BOOTSTRAP_ENV_VAR}=1 to disable auto-install)",
        paths.bin.display()
    );
    let report = bootstrap_rustup_blocking(paths)?;
    Ok(AutoBootstrapOutcome::Installed(report))
}

/// Async variant of [`auto_bootstrap_if_missing_blocking`]. Call this when you
/// already own a tokio runtime context — avoids spawning a worker thread for
/// the download.
pub async fn auto_bootstrap_if_missing(
    paths: &SoldrPaths,
) -> Result<AutoBootstrapOutcome, SoldrError> {
    if let Some(p) = discover_rustup(paths) {
        return Ok(AutoBootstrapOutcome::AlreadyInstalled(p));
    }
    if no_bootstrap_opt_out() {
        return Ok(AutoBootstrapOutcome::OptedOut);
    }
    eprintln!(
        "soldr: rustup not found; bootstrapping into {} \
         (set {NO_BOOTSTRAP_ENV_VAR}=1 to disable auto-install)",
        paths.bin.display()
    );
    let report = bootstrap_rustup(paths).await?;
    Ok(AutoBootstrapOutcome::Installed(report))
}

/// Download `rustup-init` for the current host and run it against the
/// soldr-managed `CARGO_HOME` / `RUSTUP_HOME`. Idempotent: a second call when
/// the resulting `rustup` binary already exists short-circuits without going
/// to the network.
pub async fn bootstrap_rustup(paths: &SoldrPaths) -> Result<BootstrapReport, SoldrError> {
    paths.ensure_dirs()?;

    let rustup_path = managed_rustup_path(paths);
    if rustup_path.is_file() {
        return Ok(BootstrapReport {
            rustup_path,
            already_installed: true,
            source_url: None,
        });
    }

    let url = rustup_init_download_url()?;
    let installer = download_rustup_init(&paths.cache, &url).await?;

    let cargo_home = managed_cargo_home(paths);
    let rustup_home = managed_rustup_home(paths);
    std::fs::create_dir_all(&cargo_home)?;
    std::fs::create_dir_all(&rustup_home)?;

    let mut command = std::process::Command::new(&installer);
    command.args([
        "-y",
        "--no-modify-path",
        "--default-toolchain",
        "none",
        "--profile",
        "minimal",
    ]);
    command.env("CARGO_HOME", &cargo_home);
    command.env("RUSTUP_HOME", &rustup_home);
    let status = command.status().map_err(|err| {
        SoldrError::Other(format!(
            "bootstrap: failed to launch rustup-init ({}): {err}",
            installer.display()
        ))
    })?;
    if !status.success() {
        return Err(SoldrError::Other(format!(
            "bootstrap: rustup-init exited with status {status}"
        )));
    }

    // rustup-init installs `rustup` (and shims) into `$CARGO_HOME/bin`. Copy
    // the rustup binary into the soldr-managed `bin/` so it lives in a
    // predictable, soldr-owned location. The shims and toolchain state stay
    // under `cargo_home` / `rustup_home`.
    let installed = cargo_home.join("bin").join(rustup_filename());
    if !installed.is_file() {
        return Err(SoldrError::Other(format!(
            "bootstrap: expected rustup at {} after rustup-init ran but the file is missing",
            installed.display()
        )));
    }
    std::fs::copy(&installed, &rustup_path).map_err(|err| {
        SoldrError::Other(format!(
            "bootstrap: failed to copy {} -> {}: {err}",
            installed.display(),
            rustup_path.display()
        ))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&rustup_path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&rustup_path, perms)?;
    }

    Ok(BootstrapReport {
        rustup_path,
        already_installed: false,
        source_url: Some(url),
    })
}

/// Sync entry point used by sync callers (e.g. `resolve_toolchain_binary`).
/// Spawns a current-thread runtime on a dedicated worker OS thread so it
/// composes safely whether or not the caller is already inside a tokio runtime.
pub fn bootstrap_rustup_blocking(paths: &SoldrPaths) -> Result<BootstrapReport, SoldrError> {
    let paths_clone = SoldrPaths::with_root(paths.root.clone());
    run_blocking(async move { bootstrap_rustup(&paths_clone).await })
}

/// Path the bootstrap routine copies `rustup` to (under the soldr-managed `bin/`).
pub fn managed_rustup_path(paths: &SoldrPaths) -> PathBuf {
    paths.bin.join(rustup_filename())
}

/// Where `rustup-init` lays out the toolchain it manages. Callers can pass
/// these to child rustup invocations via `CARGO_HOME` / `RUSTUP_HOME` so the
/// soldr-managed state doesn't bleed into the user's `~/.cargo` / `~/.rustup`.
pub fn managed_cargo_home(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("cargo")
}

pub fn managed_rustup_home(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("rustup")
}

fn rustup_filename() -> &'static str {
    if cfg!(windows) {
        "rustup.exe"
    } else {
        "rustup"
    }
}

fn rustup_init_filename() -> &'static str {
    if cfg!(windows) {
        "rustup-init.exe"
    } else {
        "rustup-init"
    }
}

/// Compute the URL for rustup-init based on the current host. Respects the
/// `SOLDR_RUSTUP_INIT_URL_OVERRIDE` and `SOLDR_RUSTUP_INIT_TRIPLE_OVERRIDE`
/// env vars used by tests.
pub fn rustup_init_download_url() -> Result<String, SoldrError> {
    if let Ok(override_url) = std::env::var(RUSTUP_INIT_URL_ENV_VAR) {
        let trimmed = override_url.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let triple = rustup_init_host_triple()?;
    let suffix = if triple.contains("-windows-") {
        ".exe"
    } else {
        ""
    };
    Ok(format!(
        "https://static.rust-lang.org/rustup/dist/{triple}/rustup-init{suffix}"
    ))
}

/// Host triple in rustup-init's URL namespace. Mirrors
/// `rustup_init_target_triple()` in the Python script
/// (`ensure_rust_toolchain.py`) — rustup-init uses `*-unknown-linux-gnu` and
/// `*-pc-windows-msvc`, not the abstract MSVC default soldr's `TargetTriple`
/// resolves at build time.
pub fn rustup_init_host_triple() -> Result<String, SoldrError> {
    if let Ok(override_triple) = std::env::var(RUSTUP_INIT_TRIPLE_ENV_VAR) {
        let trimmed = override_triple.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    Ok(match (os, arch) {
        ("windows", "x86_64") => "x86_64-pc-windows-msvc".to_string(),
        ("windows", "aarch64") => "aarch64-pc-windows-msvc".to_string(),
        ("windows", "x86") => "i686-pc-windows-msvc".to_string(),
        ("macos", "x86_64") => "x86_64-apple-darwin".to_string(),
        ("macos", "aarch64") => "aarch64-apple-darwin".to_string(),
        ("linux", "x86_64") if cfg!(target_env = "musl") => "x86_64-unknown-linux-musl".to_string(),
        ("linux", "aarch64") if cfg!(target_env = "musl") => {
            "aarch64-unknown-linux-musl".to_string()
        }
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu".to_string(),
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu".to_string(),
        ("linux", "x86") => "i686-unknown-linux-gnu".to_string(),
        _ => {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "bootstrap: unsupported host for rustup-init: os={os}, arch={arch}"
            )));
        }
    })
}

async fn download_rustup_init(cache_dir: &Path, url: &str) -> Result<PathBuf, SoldrError> {
    std::fs::create_dir_all(cache_dir)?;
    let destination = cache_dir.join(rustup_init_filename());

    let client = http_client()?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| SoldrError::Network(format!("bootstrap: GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "bootstrap: GET {url} -> HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SoldrError::Network(format!("bootstrap: read body {url}: {e}")))?;

    let sha256 = sha256_of(&bytes);

    let store = PinnedChecksumStore::from_env()?;
    let mode = TrustMode::from_env();
    let outcome = verify_download(
        RUSTUP_INIT_TOOL_NAME,
        RUSTUP_INIT_PSEUDO_VERSION,
        rustup_init_filename(),
        &sha256,
        &store,
        mode,
    )?;
    match outcome {
        VerifyOutcome::Verified { sha256 } => {
            eprintln!("soldr: trust: verified rustup-init sha256={sha256}");
        }
        VerifyOutcome::Unverified { sha256 } => {
            eprintln!("soldr: trust: unverified rustup-init sha256={sha256}");
        }
    }

    let tmp = destination.with_extension("tmp");
    std::fs::write(&tmp, &bytes)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&tmp, perms)?;
    }

    if destination.exists() {
        let _ = std::fs::remove_file(&destination);
    }
    std::fs::rename(&tmp, &destination)?;
    Ok(destination)
}

fn which_on_path(tool: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    let exe_names: Vec<String> = if cfg!(windows) {
        // PATHEXT controls which extensions PATH lookups try.
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        std::iter::once(tool.to_string())
            .chain(
                pathext
                    .split(';')
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .map(|ext| format!("{tool}{ext}")),
            )
            .collect()
    } else {
        vec![tool.to_string()]
    };
    for dir in std::env::split_paths(&paths) {
        for name in &exe_names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn no_bootstrap_opt_out() -> bool {
    match std::env::var(NO_BOOTSTRAP_ENV_VAR) {
        Ok(value) => is_truthy(&value),
        Err(_) => false,
    }
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Run `fut` to completion on a fresh tokio current-thread runtime spawned on
/// a dedicated OS thread. Works whether the caller is already inside a tokio
/// runtime (it is, when reached from `#[tokio::main]`) or not.
fn run_blocking<F, T>(fut: F) -> Result<T, SoldrError>
where
    F: std::future::Future<Output = Result<T, SoldrError>> + Send + 'static,
    T: Send + 'static,
{
    let handle = std::thread::Builder::new()
        .name("soldr-bootstrap".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| SoldrError::Other(format!("bootstrap: tokio init failed: {e}")))?;
            runtime.block_on(fut)
        })
        .map_err(|e| SoldrError::Other(format!("bootstrap: spawn thread failed: {e}")))?;
    handle
        .join()
        .map_err(|_| SoldrError::Other("bootstrap: worker thread panicked".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_triple_uses_unknown_linux_gnu_on_linux_x86_64() {
        let _env_lock = test_env_lock();
        let _guard = EnvVarGuard::remove(RUSTUP_INIT_TRIPLE_ENV_VAR);
        if std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64" {
            let expected = if cfg!(target_env = "musl") {
                "x86_64-unknown-linux-musl"
            } else {
                "x86_64-unknown-linux-gnu"
            };
            assert_eq!(rustup_init_host_triple().unwrap(), expected);
        }
    }

    #[test]
    fn host_triple_uses_pc_windows_msvc_on_windows_x86_64() {
        let _env_lock = test_env_lock();
        let _guard = EnvVarGuard::remove(RUSTUP_INIT_TRIPLE_ENV_VAR);
        if std::env::consts::OS == "windows" && std::env::consts::ARCH == "x86_64" {
            assert_eq!(rustup_init_host_triple().unwrap(), "x86_64-pc-windows-msvc");
        }
    }

    #[test]
    fn host_triple_honors_override_env_var() {
        let _env_lock = test_env_lock();
        let _guard = EnvVarGuard::set(RUSTUP_INIT_TRIPLE_ENV_VAR, "aarch64-apple-darwin");
        assert_eq!(rustup_init_host_triple().unwrap(), "aarch64-apple-darwin");
    }

    #[test]
    fn download_url_appends_exe_for_windows_triple_only() {
        let _env_lock = test_env_lock();
        let _url = EnvVarGuard::remove(RUSTUP_INIT_URL_ENV_VAR);
        let _triple = EnvVarGuard::set(RUSTUP_INIT_TRIPLE_ENV_VAR, "x86_64-pc-windows-msvc");
        assert_eq!(
            rustup_init_download_url().unwrap(),
            "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe"
        );

        let _triple2 = EnvVarGuard::set(RUSTUP_INIT_TRIPLE_ENV_VAR, "x86_64-unknown-linux-gnu");
        assert_eq!(
            rustup_init_download_url().unwrap(),
            "https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init"
        );
    }

    #[test]
    fn download_url_override_short_circuits_triple_resolution() {
        let _env_lock = test_env_lock();
        let _guard = EnvVarGuard::set(
            RUSTUP_INIT_URL_ENV_VAR,
            "http://127.0.0.1:9/rustup-init-test",
        );
        assert_eq!(
            rustup_init_download_url().unwrap(),
            "http://127.0.0.1:9/rustup-init-test"
        );
    }

    #[test]
    fn is_truthy_accepts_canonical_values_only() {
        for v in ["1", "true", "TRUE", "Yes", "on", " 1 "] {
            assert!(is_truthy(v), "expected {v:?} to be truthy");
        }
        for v in ["0", "false", "no", "off", "", "maybe"] {
            assert!(!is_truthy(v), "expected {v:?} to be falsy");
        }
    }

    #[test]
    fn auto_bootstrap_reports_opted_out_when_env_var_set() {
        // Take the env lock BEFORE the PATH probe so the two
        // `which_on_path` invocations (one here, one inside
        // `discover_rustup`) see the same PATH. Without the lock, a
        // concurrent test mutating PATH between those two calls could
        // let us slip past the skip-check, then find rustup inside
        // auto_bootstrap_if_missing_blocking, then panic with
        // "expected OptedOut, got AlreadyInstalled(...)" — observed
        // when this module's tests share a process with the rest of
        // the suite after the four-crate → one-crate collapse.
        let _env_lock = test_env_lock();
        // The test only exercises the OptedOut path. Skip when rustup
        // is already discoverable on PATH — the early `if let Some` in
        // auto_bootstrap_if_missing_blocking would then return
        // AlreadyInstalled, which is correct but not what we're
        // testing.
        if which_on_path("rustup").is_some() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let _guard = EnvVarGuard::set(NO_BOOTSTRAP_ENV_VAR, "1");
        match auto_bootstrap_if_missing_blocking(&paths).unwrap() {
            AutoBootstrapOutcome::OptedOut => {}
            other => panic!("expected OptedOut, got {other:?}"),
        }
    }

    /// Module-wide lock that serialises every test in this file that
    /// touches the process-wide env. Every such test must call
    /// `test_env_lock()` at the top and bind its return value for the
    /// scope of the test. This was harmless when the rustup_init
    /// module lived in its own crate (`cargo test` gave it a dedicated
    /// process), but became necessary after the four-crate → one-crate
    /// collapse — the env mutations now share a process with every
    /// other test in `soldr-cli`.
    fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// RAII helper so tests can mutate process env without leaking the
    /// mutation across cases. Does NOT take the env lock — the
    /// enclosing test must already hold one via `test_env_lock()` for
    /// the full duration of all guards it constructs.
    struct EnvVarGuard {
        key: String,
        prior: Option<String>,
    }
    impl EnvVarGuard {
        fn set(key: &str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key: key.to_string(),
                prior,
            }
        }
        fn remove(key: &str) -> Self {
            let prior = std::env::var(key).ok();
            std::env::remove_var(key);
            Self {
                key: key.to_string(),
                prior,
            }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}
