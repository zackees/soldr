//! `rpprobe` — the operator's command-line surface (S14 / #643).
//!
//! Every subcommand is a thin shell over a `probe_diag.v1` verb, which is the
//! same verb the HTTP surface calls, which is the same `ProbeOps` function the
//! control socket dispatches to. Three front doors, one set of rules — so an
//! env value the daemon will not disclose is one `rpprobe` cannot print,
//! because the CLI was never the thing deciding.
//!
//! # This is not the broker's `DUMP`
//!
//! `rpprobe dump` and the broker admin `DUMP` verb share a word and nothing
//! else: different daemon, different wire, different authorization. Routing
//! one through the other would mean a probe capture inherited the broker's
//! ACL, which is not the ACL this data is under.
//!
//! # Exit codes
//!
//! `0` on success, non-zero on anything else, so `rpprobe doctor` and shell
//! scripts can branch on the result instead of grepping output.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

pub mod commands;
pub mod render;
pub mod transport;

#[cfg(test)]
mod tests;

/// Default page size for the listing commands.
///
/// The query engine refuses an absent limit by design, so the CLI has to pick
/// one. It is visible here and overridable with `--limit`, rather than being
/// an invisible default inside the engine where nobody could see it.
pub const DEFAULT_LIMIT: u32 = 100;

/// Diagnose and inspect processes registered with `rpprobed`.
#[derive(Debug, Parser)]
#[command(name = "rpprobe", version, about, long_about = None)]
pub struct Cli {
    /// Path to the daemon's discovery file, or the directory holding it.
    #[arg(long, global = true)]
    pub discovery: Option<PathBuf>,

    /// Emit JSON instead of a human-readable table.
    #[arg(long, global = true)]
    pub json: bool,

    /// Reach the daemon over HTTP rather than the control socket.
    ///
    /// The socket is authorized by peer credentials and sends no secret;
    /// HTTP sends the bearer token. Prefer the socket unless the response is
    /// too large for its frame cap.
    #[arg(long, global = true)]
    pub http: bool,

    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// List processes registered with the daemon.
    Ps {
        /// Glob on process name, e.g. `*worker*`.
        #[arg(long)]
        name: Option<String>,
        /// Also list processes that never registered.
        #[arg(long)]
        include_unregistered: bool,
        /// Show environment values the target allowlisted.
        #[arg(long)]
        env: bool,
        /// Maximum rows.
        #[arg(long)]
        limit: Option<u32>,
    },

    /// Capture an all-thread stack from one or more processes.
    Dump {
        /// Target process id. Omit when selecting with `--name`.
        pid: Option<u32>,
        /// Glob on process name.
        #[arg(long)]
        name: Option<String>,
        /// Exact instance name.
        #[arg(long)]
        instance: Option<String>,
        /// Capture every match rather than refusing an ambiguous selection.
        #[arg(long)]
        all: bool,
        /// Maximum native frames per thread.
        #[arg(long, default_value_t = 128)]
        max_depth: u32,
        /// Capture an unenrolled target with external tools.
        ///
        /// For the process that never called `probe::install()` — usually
        /// the one you most need a stack from. Never elevates and never
        /// changes `ptrace_scope`; where OS policy refuses, it says so and
        /// explains why.
        #[arg(long)]
        force: bool,
    },

    /// Capture an all-thread stack from exactly one process.
    Snapshot {
        /// Target process id.
        pid: u32,
        /// Maximum native frames per thread.
        #[arg(long, default_value_t = 128)]
        max_depth: u32,
    },

    /// Browse durable crash history.
    Crashes {
        /// Exact application class.
        #[arg(long)]
        class: Option<String>,
        /// `LIKE` pattern on application class, e.g. `clud%`.
        #[arg(long)]
        class_like: Option<String>,
        /// Exact crash signature.
        #[arg(long)]
        signature: Option<String>,
        /// Roll up by signature instead of listing records.
        #[arg(long)]
        stats: bool,
        /// Maximum rows. Ignored with `--stats`.
        #[arg(long)]
        limit: Option<u32>,
    },

    /// Download one artifact by id.
    ///
    /// Always over HTTP: an artifact is routinely larger than the control
    /// socket's frame cap, which is why the streaming endpoint exists.
    Fetch {
        /// Artifact id, as reported by `rpprobe crashes`.
        id: i64,
        /// Where to write it. Defaults to `probe-artifact-<id>.bin`.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Capture a CPU profile of the daemon's host and save its exports.
    ///
    /// Always over HTTP: profiling is a daemon-side operation whose result is
    /// an artifact, and artifacts are what the socket's frame cap keeps off
    /// it.
    Profile {
        /// Seconds to sample for. Clamped by the daemon to its hard ceiling.
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Sampling frequency in hertz.
        #[arg(long)]
        hz: Option<u32>,
        /// Which export to save: pprof, json (Firefox), or collapsed.
        #[arg(long, default_value = "collapsed")]
        format: String,
        /// Where to write it. Defaults to `profile-<id>.<format>`.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Check that the probe stack is actually usable, and say what is not.
    Doctor,
}

/// Run the CLI and return a process exit code.
///
/// Separated from `main` so tests can drive it in-process.
pub fn run(cli: Cli) -> i32 {
    match commands::dispatch(&cli) {
        Ok(output) => {
            print!("{output}");
            0
        }
        Err(error) => {
            eprintln!("rpprobe: {error}");
            1
        }
    }
}
