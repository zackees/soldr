// runpm — PM2-style process supervisor CLI (Phase 1: skeleton).
// Daemon stubs respond OK; real lifecycle lands in Phase 2. See issue #106.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use running_process::boot_autostart;
use running_process::client::{connect_or_start, ClientError, DaemonClient};
use running_process::maintenance::run_release_handles;
use running_process::proto::daemon::{DaemonResponse, ServiceConfig, ServiceState, StatusCode};
use running_process::runpm_config::{AppConfig, RunpmConfig};

/// Polling interval used by `runpm logs --follow`. Documented in
/// the CLI help text — this is best-effort, not real-time streaming.
const LOGS_FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Cap the `--follow` poll request to a generous tail so we never miss
/// lines written between polls. The daemon-side budget (~64 KiB) still
/// applies, so this is a safe upper bound.
const LOGS_FOLLOW_TAIL_LINES: u32 = 10_000;

#[derive(Parser)]
#[command(
    name = "runpm",
    about = "PM2-style process supervisor for running-process. See https://github.com/zackees/running-process/issues/106"
)]
struct Cli {
    /// Spawn the daemon detached and exit
    #[arg(long, global = true)]
    start_daemon: bool,

    /// Stop the daemon (alias for `kill`)
    #[arg(long, global = true)]
    stop_daemon: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a supervised service
    Start {
        /// Command to run (executable plus arguments). Optional when
        /// `--config` is passed or a `runpm.toml` is auto-discovered.
        #[arg(num_args = 0..)]
        cmd: Vec<String>,
        /// Service name (defaults to basename of `cmd[0]`)
        #[arg(long)]
        name: Option<String>,
        /// Working directory
        #[arg(long)]
        cwd: Option<String>,
        /// Environment variables (KEY=VALUE), repeatable
        #[arg(long = "env")]
        env: Vec<String>,
        /// Disable auto-restart on exit
        #[arg(long)]
        no_autorestart: bool,
        /// Maximum restart attempts (0 = unlimited)
        #[arg(long, default_value_t = 0u32)]
        max_restarts: u32,
        /// Batch-start every `[[app]]` entry from a `runpm.toml` file.
        /// When set, all other start flags are ignored. See #428.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
    /// Stop a supervised service
    Stop {
        /// Service name, id, or "all"
        target: String,
    },
    /// Restart a supervised service
    Restart {
        /// Service name, id, or "all"
        target: String,
    },
    /// Delete a service from the registry
    Delete {
        /// Service name, id, or "all"
        target: String,
    },
    /// List all supervised services
    #[command(alias = "ls", alias = "status")]
    List {
        /// Emit JSON instead of a human-readable table
        #[arg(long)]
        json: bool,
    },
    /// Show details about a single service
    #[command(alias = "describe")]
    Show {
        /// Service name or id
        target: String,
    },
    /// Show buffered logs for a service
    Logs {
        /// Service name or id (default: all)
        target: Option<String>,
        /// Number of trailing lines
        #[arg(long, default_value_t = 100u32)]
        lines: u32,
        /// Follow log output. Polls the daemon every 500ms; not real-time
        /// streaming. Ctrl-C to exit.
        #[arg(long)]
        follow: bool,
    },
    /// Flush log buffers for a service
    Flush {
        /// Service name or id (default: all)
        target: Option<String>,
    },
    /// Persist the current service set to a snapshot
    Save,
    /// Restore services from the latest snapshot
    Resurrect,
    /// Install runpm to start at boot (per-OS: systemd user unit on
    /// Linux, launchd LaunchAgent on macOS, Task Scheduler ONLOGON task
    /// on Windows). See #427.
    Startup,
    /// Uninstall runpm boot integration.
    Unstartup,
    /// Ping the daemon
    Ping,
    /// Stop the running daemon
    Kill,
    /// Maintenance subcommands (#228 Phase 1)
    Maintenance {
        #[command(subcommand)]
        command: MaintenanceCommands,
    },
}

#[derive(Subcommand)]
enum MaintenanceCommands {
    /// Ask live daemons to release any open handles under `PATH`.
    ///
    /// POSIX: no-op (delete-on-close semantics make this unnecessary).
    /// Windows: Phase 1 ships a scaffold; the full handler ships in
    /// Phase 2 once the manifest registry exists. See issue #230.
    ReleaseHandles {
        /// Filesystem path that the caller wants to rm -rf.
        #[arg(long)]
        path: PathBuf,
        /// Emit a JSON document on stdout instead of a human-readable line.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.start_daemon {
        return run_to_exit(cmd_start_daemon());
    }
    if cli.stop_daemon {
        return run_to_exit(cmd_kill());
    }

    let Some(command) = cli.command else {
        eprintln!("error: a subcommand is required (try `runpm --help`)");
        return ExitCode::from(2);
    };

    let result = match command {
        Commands::Start {
            cmd,
            name,
            cwd,
            env,
            no_autorestart,
            max_restarts,
            config,
        } => cmd_start(cmd, name, cwd, env, !no_autorestart, max_restarts, config),
        Commands::Stop { target } => cmd_simple_target("stop", &target, |c, t| c.service_stop(t)),
        Commands::Restart { target } => {
            cmd_simple_target("restart", &target, |c, t| c.service_restart(t))
        }
        Commands::Delete { target } => {
            cmd_simple_target("delete", &target, |c, t| c.service_delete(t))
        }
        Commands::List { json } => cmd_list(json),
        Commands::Show { target } => cmd_show(&target),
        Commands::Logs {
            target,
            lines,
            follow,
        } => cmd_logs(target.as_deref().unwrap_or(""), lines, follow),
        Commands::Flush { target } => {
            cmd_simple_target("flush", target.as_deref().unwrap_or("all"), |c, t| {
                c.service_flush(t)
            })
        }
        Commands::Save => cmd_no_arg("save", |c| c.service_save()),
        Commands::Resurrect => cmd_no_arg("resurrect", |c| c.service_resurrect()),
        Commands::Startup => cmd_startup(),
        Commands::Unstartup => cmd_unstartup(),
        Commands::Ping => cmd_ping(),
        Commands::Kill => cmd_kill(),
        Commands::Maintenance { command } => cmd_maintenance(command),
    };

    run_to_exit(result)
}

fn run_to_exit(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon lifecycle
// ---------------------------------------------------------------------------

fn cmd_start_daemon() -> Result<()> {
    let mut client = connect_or_start(None).context("failed to connect to or start daemon")?;
    let resp = client.ping().map_err(client_err)?;
    let server_time = resp.ping.map(|p| p.server_time_ms).unwrap_or(0);
    let status = client.status().map_err(client_err)?;
    if let Some(s) = status.status {
        println!(
            "daemon ready (server_time_ms={server_time}, socket={})",
            s.socket_path
        );
    } else {
        println!("daemon ready (server_time_ms={server_time})");
    }
    Ok(())
}

fn cmd_ping() -> Result<()> {
    let mut client = connect()?;
    let resp = client.ping().map_err(client_err)?;
    let server_time = resp.ping.map(|p| p.server_time_ms).unwrap_or(0);
    println!("OK (server_time_ms={server_time})");
    Ok(())
}

fn cmd_kill() -> Result<()> {
    let mut client = match DaemonClient::connect(None) {
        Ok(c) => c,
        Err(_) => {
            println!("daemon is not running");
            return Ok(());
        }
    };
    client.shutdown(true, 5.0).map_err(client_err)?;
    println!("daemon shutting down");
    Ok(())
}

// ---------------------------------------------------------------------------
// Service subcommands
// ---------------------------------------------------------------------------

fn cmd_start(
    cmd: Vec<String>,
    name: Option<String>,
    cwd: Option<String>,
    env_args: Vec<String>,
    autorestart: bool,
    max_restarts: u32,
    config: Option<PathBuf>,
) -> Result<()> {
    // Explicit `--config <path>`: batch-start every `[[app]]` and ignore
    // the other start flags entirely (#428).
    if let Some(path) = config {
        return cmd_start_from_config(&path);
    }

    // No explicit cmd? Try the auto-discovery fallback before erroring.
    if cmd.is_empty() {
        if let Some(discovered) = discover_runpm_toml() {
            return cmd_start_from_config(&discovered);
        }
        return Err(anyhow!(
            "missing cmd: pass a command, --config <path>, or place a runpm.toml in the current directory"
        ));
    }

    let env = parse_env(&env_args)?;
    let resolved_name = match name {
        Some(n) => n,
        None => default_name_from(&cmd[0])?,
    };

    let svc_config = ServiceConfig {
        name: resolved_name,
        cmd,
        cwd: cwd.unwrap_or_default(),
        env,
        autorestart,
        max_restarts,
        restart_delay_ms: 0,
        kill_timeout_ms: 0,
        min_uptime_ms: 0,
    };

    let mut client = connect()?;
    let resp = client.service_start(svc_config).map_err(client_err)?;
    ensure_ok("start", &resp)?;
    if let Some(svc) = resp.service_start.and_then(|r| r.service) {
        println!(
            "started '{}' (id={}, pid={}, status={})",
            svc.name, svc.id, svc.pid, svc.status
        );
    }
    Ok(())
}

/// Load a `runpm.toml`, register every `[[app]]` with the daemon, and
/// return an error if any of them failed (so the CLI exits 1). One
/// failed entry never strands the rest of the batch.
fn cmd_start_from_config(path: &Path) -> Result<()> {
    let parsed = RunpmConfig::load(path)
        .with_context(|| format!("failed to load runpm config {}", path.display()))?;
    let total = parsed.app.len();
    if total == 0 {
        println!("no apps to start in {}", path.display());
        return Ok(());
    }

    let mut client = connect()?;
    let mut started = 0usize;
    let mut failed = 0usize;
    for app in &parsed.app {
        match start_single_app_from_config(&mut client, path, app) {
            Ok(()) => started += 1,
            Err(e) => {
                eprintln!("error starting '{}': {e:#}", app.name);
                failed += 1;
            }
        }
    }
    println!("started {started} of {total} apps from {}", path.display());
    if failed == 0 {
        Ok(())
    } else {
        Err(anyhow!(
            "{failed} of {total} apps failed to start from {}",
            path.display()
        ))
    }
}

/// Dispatch a single `[[app]]` entry to the daemon. Each failure is the
/// caller's problem to report so the batch can continue past it.
fn start_single_app_from_config(
    client: &mut DaemonClient,
    config_path: &Path,
    app: &AppConfig,
) -> Result<()> {
    let cwd = RunpmConfig::resolve_cwd(config_path, &app.cwd).unwrap_or_default();
    let svc_config = ServiceConfig {
        name: app.name.clone(),
        cmd: app.cmd.clone(),
        cwd,
        env: app.env.clone(),
        autorestart: app.autorestart,
        max_restarts: app.max_restarts.unwrap_or(0),
        restart_delay_ms: app.restart_delay_ms.unwrap_or(0),
        kill_timeout_ms: app.kill_timeout_ms.unwrap_or(0),
        min_uptime_ms: app.min_uptime_ms.unwrap_or(0),
    };
    let resp = client.service_start(svc_config).map_err(client_err)?;
    ensure_ok("start", &resp)?;
    if let Some(svc) = resp.service_start.and_then(|r| r.service) {
        println!("started '{}' (id={})", svc.name, svc.id);
    } else {
        // Daemon returned OK without a populated payload — surface the
        // ack so the operator sees forward progress.
        println!("started '{}'", app.name);
    }
    Ok(())
}

/// Look for a `runpm.toml` in the current directory or the per-user
/// config directory, returning the first match. If both exist, prefer
/// the in-repo one and note the override on stderr.
fn discover_runpm_toml() -> Option<PathBuf> {
    let cwd_path = std::env::current_dir().ok().map(|p| p.join("runpm.toml"));
    let cwd_hit = cwd_path.as_ref().filter(|p| p.is_file()).cloned();
    let user_hit = user_config_runpm_toml().filter(|p| p.is_file());

    match (cwd_hit, user_hit) {
        (Some(cwd), Some(user)) => {
            eprintln!(
                "note: using {} (also found {})",
                cwd.display(),
                user.display()
            );
            Some(cwd)
        }
        (Some(cwd), None) => Some(cwd),
        (None, Some(user)) => Some(user),
        (None, None) => None,
    }
}

/// Resolve the per-user runpm config location:
/// - Linux/macOS: `$XDG_CONFIG_HOME/runpm/config.toml` (falls back to
///   `~/.config/runpm/config.toml`).
/// - Windows: `%APPDATA%\runpm\config.toml`.
fn user_config_runpm_toml() -> Option<PathBuf> {
    let base = dirs::config_dir()?;
    Some(base.join("runpm").join("config.toml"))
}

fn cmd_list(json: bool) -> Result<()> {
    let mut client = connect()?;
    let resp = client.service_list().map_err(client_err)?;
    ensure_ok("list", &resp)?;
    let services = resp.service_list.map(|r| r.services).unwrap_or_default();
    if json {
        print_services_json(&services);
    } else {
        print_services_table(&services);
    }
    Ok(())
}

fn cmd_show(target: &str) -> Result<()> {
    let mut client = connect()?;
    let resp = client.service_describe(target).map_err(client_err)?;
    ensure_ok("show", &resp)?;
    match resp.service_describe.and_then(|r| r.service) {
        Some(svc) => print_service_detail(&svc),
        None => println!("no such service: {target}"),
    }
    Ok(())
}

fn cmd_logs(target: &str, lines: u32, follow: bool) -> Result<()> {
    let mut client = connect()?;
    let resp = client
        .service_logs(target, lines, follow)
        .map_err(client_err)?;
    ensure_ok("logs", &resp)?;
    let mut last_text = resp.service_logs.map(|p| p.log_text).unwrap_or_default();
    if !last_text.is_empty() {
        print!("{last_text}");
        if !last_text.ends_with('\n') {
            println!();
        }
    }

    if !follow {
        return Ok(());
    }

    // Ctrl-C is handled by the process default (SIGINT terminates) so
    // the loop simply polls until it's killed. This is intentionally
    // simple — see the polling-latency caveat documented on `--follow`.
    loop {
        std::thread::sleep(LOGS_FOLLOW_POLL_INTERVAL);
        let resp = match client.service_logs(target, LOGS_FOLLOW_TAIL_LINES, false) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error polling logs: {e}");
                break;
            }
        };
        if resp.code != StatusCode::Ok as i32 {
            eprintln!(
                "error polling logs: {}",
                if resp.message.is_empty() {
                    format!("daemon returned code {}", resp.code)
                } else {
                    resp.message
                }
            );
            break;
        }
        let current = resp.service_logs.map(|p| p.log_text).unwrap_or_default();
        if let Some(delta) = compute_log_delta(&last_text, &current) {
            print!("{delta}");
            if !delta.ends_with('\n') {
                println!();
            }
        }
        last_text = current;
    }
    Ok(())
}

/// Given the previous full log tail and the current full log tail, return
/// only the new content. Falls back to printing the whole `current` text
/// when there's no overlap (e.g. operator ran `runpm flush` between polls,
/// which legitimately resets the tail).
fn compute_log_delta(previous: &str, current: &str) -> Option<String> {
    if current.is_empty() {
        return None;
    }
    if previous.is_empty() {
        return Some(current.to_string());
    }
    if let Some(rest) = current.strip_prefix(previous) {
        if rest.is_empty() {
            return None;
        }
        return Some(rest.to_string());
    }
    // No prefix match — the tail rotated (flush or log truncation). Show
    // the whole thing and let the operator see the discontinuity.
    Some(current.to_string())
}

fn cmd_simple_target<F>(label: &str, target: &str, call: F) -> Result<()>
where
    F: FnOnce(&mut DaemonClient, &str) -> Result<DaemonResponse, ClientError>,
{
    let mut client = connect()?;
    let resp = call(&mut client, target).map_err(client_err)?;
    ensure_ok(label, &resp)?;
    let count = match label {
        "stop" => resp.service_stop.as_ref().map(|r| r.stopped_count),
        "restart" => resp.service_restart.as_ref().map(|r| r.restarted_count),
        "delete" => resp.service_delete.as_ref().map(|r| r.deleted_count),
        _ => None,
    };
    match count {
        Some(n) => println!("{label}: {n} service(s)"),
        None => println!("OK: {label}"),
    }
    Ok(())
}

fn cmd_no_arg<F>(label: &str, call: F) -> Result<()>
where
    F: FnOnce(&mut DaemonClient) -> Result<DaemonResponse, ClientError>,
{
    let mut client = connect()?;
    let resp = call(&mut client).map_err(client_err)?;
    print_status(label, &resp)
}

/// `runpm startup` — install per-OS boot autostart for the daemon.
///
/// The daemon binary path is resolved by looking for
/// `running-process-daemon` next to the currently-running `runpm`
/// executable (the layout `cargo install running-process` produces);
/// if that lookup fails we fall back to a `PATH` lookup so an operator
/// who installed only the daemon binary via a system package still
/// gets a working install.
fn cmd_startup() -> Result<()> {
    let daemon = resolve_daemon_binary()?;
    let path = boot_autostart::install(&daemon)
        .map_err(|e| anyhow!("failed to install boot autostart: {e}"))?;
    println!("runpm startup: installed boot autostart at {path}");
    Ok(())
}

/// `runpm unstartup` — remove per-OS boot autostart for the daemon.
fn cmd_unstartup() -> Result<()> {
    boot_autostart::uninstall().map_err(|e| anyhow!("failed to uninstall boot autostart: {e}"))?;
    println!("runpm unstartup: removed boot autostart");
    Ok(())
}

/// Resolve the absolute path of the `running-process-daemon` binary the
/// boot autostart unit should launch. Strategy:
///   1. Look next to the currently-running `runpm` executable for
///      `running-process-daemon{exe_suffix}`. This is the layout that
///      `cargo install running-process` and any system package would
///      produce.
///   2. Fall back to `which running-process-daemon` (via `Command::new`
///      with no path qualifier — the OS PATH search does the heavy lifting).
fn resolve_daemon_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("could not resolve current_exe for runpm")?;
    let sibling = exe.parent().map(|p| {
        #[cfg(windows)]
        {
            p.join("running-process-daemon.exe")
        }
        #[cfg(not(windows))]
        {
            p.join("running-process-daemon")
        }
    });
    if let Some(p) = sibling {
        if p.is_file() {
            return Ok(p);
        }
    }
    // Fall back to a bare name; whatever PATH resolves wins. We do NOT
    // probe with `which` because the daemon's not actually invoked
    // here, just referenced; if the PATH lookup turns out wrong, the
    // operator will see it the first time the unit fires.
    Ok(PathBuf::from(if cfg!(windows) {
        "running-process-daemon.exe"
    } else {
        "running-process-daemon"
    }))
}

// ---------------------------------------------------------------------------
// Maintenance subcommands (#228 Phase 1)
// ---------------------------------------------------------------------------

fn cmd_maintenance(command: MaintenanceCommands) -> Result<()> {
    match command {
        MaintenanceCommands::ReleaseHandles { path, json } => cmd_release_handles(path, json),
    }
}

fn cmd_release_handles(path: PathBuf, json: bool) -> Result<()> {
    let outcome = run_release_handles(&path)
        .with_context(|| format!("release-handles failed for {}", path.display()))?;
    if json {
        println!("{}", outcome.to_json());
    } else {
        println!("{}", outcome.message);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn connect() -> Result<DaemonClient> {
    DaemonClient::connect(None)
        .map_err(|_| anyhow!("daemon is not running (try `runpm --start-daemon`)"))
}

fn client_err(err: ClientError) -> anyhow::Error {
    anyhow!(err.to_string())
}

fn print_status(label: &str, resp: &DaemonResponse) -> Result<()> {
    ensure_ok(label, resp)?;
    println!("OK: {label}");
    Ok(())
}

/// Return an error if the daemon responded with a non-OK status code.
fn ensure_ok(label: &str, resp: &DaemonResponse) -> Result<()> {
    if resp.code == StatusCode::Ok as i32 {
        Ok(())
    } else {
        Err(anyhow!(
            "{}",
            if resp.message.is_empty() {
                format!("{label} failed (code={})", resp.code)
            } else {
                resp.message.clone()
            }
        ))
    }
}

// ---------------------------------------------------------------------------
// Service rendering
// ---------------------------------------------------------------------------

fn print_services_table(services: &[ServiceState]) {
    if services.is_empty() {
        println!("no services");
        return;
    }
    println!(
        "{:<4} {:<20} {:<10} {:<8} {:<8} COMMAND",
        "ID", "NAME", "STATUS", "PID", "RESTARTS"
    );
    for s in services {
        let command = s
            .config
            .as_ref()
            .map(|c| c.cmd.join(" "))
            .unwrap_or_default();
        println!(
            "{:<4} {:<20} {:<10} {:<8} {:<8} {}",
            s.id,
            s.name,
            render_status(&s.status),
            s.pid,
            s.restart_count,
            command
        );
    }
}

/// Render a status string for the `runpm list` table. `errored` (the
/// supervisor gave up after `max_restarts`) is surfaced distinctly so an
/// operator can spot it at a glance — plain text for now (no ANSI), since
/// the rest of the CLI is also plain text.
fn render_status(status: &str) -> String {
    // Today this is a passthrough; the function exists so a future ANSI
    // coloring pass has a single edit point.
    status.to_string()
}

fn print_service_detail(s: &ServiceState) {
    println!("name:          {}", s.name);
    println!("id:            {}", s.id);
    println!("status:        {}", s.status);
    println!("pid:           {}", s.pid);
    println!("restart_count: {}", s.restart_count);
    println!("last_exit:     {}", s.last_exit_code);
    if s.last_exited_at != 0.0 {
        println!("last_exited_at: {} (epoch_seconds)", s.last_exited_at);
    }
    if s.last_started_at != 0.0 {
        println!("last_started_at: {} (epoch_seconds)", s.last_started_at);
    }
    if let Some(c) = &s.config {
        println!("command:       {}", c.cmd.join(" "));
        if !c.cwd.is_empty() {
            println!("cwd:           {}", c.cwd);
        }
        println!("autorestart:   {}", c.autorestart);
        println!("max_restarts:  {}", c.max_restarts);
    }
}

fn print_services_json(services: &[ServiceState]) {
    let values: Vec<serde_json::Value> = services
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "name": s.name,
                "status": s.status,
                "pid": s.pid,
                "restart_count": s.restart_count,
                "last_started_at": s.last_started_at,
                "last_exited_at": s.last_exited_at,
                "last_exit_code": s.last_exit_code,
                "command": s.config.as_ref().map(|c| c.cmd.clone()).unwrap_or_default(),
                "cwd": s.config.as_ref().map(|c| c.cwd.clone()).unwrap_or_default(),
                "autorestart": s.config.as_ref().map(|c| c.autorestart).unwrap_or(false),
                "max_restarts": s.config.as_ref().map(|c| c.max_restarts).unwrap_or(0),
            })
        })
        .collect();
    match serde_json::to_string_pretty(&values) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("failed to serialize JSON: {e}"),
    }
}

fn parse_env(args: &[String]) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for entry in args {
        let (key, value) = entry
            .split_once('=')
            .ok_or_else(|| anyhow!("--env value `{entry}` must be in KEY=VALUE form"))?;
        if key.is_empty() {
            return Err(anyhow!("--env value `{entry}` has empty key"));
        }
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

fn default_name_from(cmd: &str) -> Result<String> {
    let stem = Path::new(cmd)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty());
    stem.map(|s| s.to_string())
        .ok_or_else(|| anyhow!("could not derive default --name from `{cmd}`; pass --name"))
}
