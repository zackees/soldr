//! clap derives for the `soldr` binary (top-level `Cli` + every
//! subcommand enum / args struct). Extracted from `main.rs` to keep
//! that file under the soldr-wide 1 000-LOC source budget enforced by
//! `ci/hooks/loc_guard.py` (warn >1K, block >1.5K). The split is
//! purely organisational — `main.rs` re-imports each type with
//! `use cli_args::*` so the dispatch path is unchanged.

use crate::{optimize, save_load};

#[derive(clap::Parser)]
#[command(name = "soldr", version, about = "Instant tools. Instant builds.")]
pub(crate) struct Cli {
    /// Disable soldr's compilation cache for this invocation
    #[arg(long)]
    pub(crate) no_cache: bool,
    /// Pick the zccache binary backing the compilation cache.
    ///
    /// `managed` (default) fetches the pinned zccache release into
    /// `~/.soldr/`. `system` uses the `zccache` already on PATH
    /// (must have `zccache-daemon` and `zccache-fp` as siblings).
    #[arg(long, value_enum, default_value_t = ZccacheSourceArg::Managed, value_name = "SOURCE")]
    pub(crate) zccache: ZccacheSourceArg,
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ZccacheSourceArg {
    /// Use the soldr-managed zccache release (default).
    #[default]
    Managed,
    /// Use the `zccache` binary already installed on PATH.
    System,
}

/// Flat list of every built-in soldr verb that clap recognizes,
/// PLUS aliases (e.g. `update-zccache` for `install-zccache`,
/// `purge-targets` for `gc`). Used by the fuzzy-match suggestion
/// path in `Commands::External` (issue #412) to detect typos /
/// pre-rename verbs that fell through to the external-tool fetch.
///
/// Must stay in sync with the `Commands` enum + `#[command(alias = ...)]`
/// attributes below. A unit test in `main_tests.rs` walks the const
/// against clap's discovered subcommands and fails when they drift,
/// so adding a new verb here without updating the enum (or vice
/// versa) trips the build.
pub(crate) const SOLDR_BUILTIN_VERBS: &[&str] = &[
    "cargo",
    "cook",
    "rustc",
    "rustfmt",
    "clippy-driver",
    "rustdoc",
    "rust-gdb",
    "rust-lldb",
    "rust-analyzer",
    "status",
    "clean",
    "purge",
    "config",
    "cache",
    "version",
    "gc",
    "purge-targets", // alias of `gc`
    "rustup",
    "toolchain",
    "bootstrap",
    "doctor",
    "optimize",
    "session-start",
    "session-end",
    "install-zccache",
    "update-zccache", // alias of `install-zccache`
    "save",
    "load",
    "daemon",
];

#[derive(clap::Subcommand)]
pub(crate) enum Commands {
    /// Run Cargo through soldr's front door
    Cargo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Content-addressable dep prebuild via the bundled `cargo-chef`
    /// (issue #359). Splits a project build into a recipe phase
    /// (`cargo chef prepare`) and a stub-project compile phase
    /// (`cargo chef cook`) so the dep set can be cached as an output
    /// layer (Docker), a tarball (CI), or just a warm `target/` (local
    /// dev) that survives source-code commits. Routes both phases
    /// through the cargo front door so zccache, `ZCCACHE_PATH_REMAP=auto`,
    /// and the soldr-managed toolchain homes all apply.
    ///
    /// Recognised flags (everything else: pass after `--`):
    /// `--release`, `--target <triple>`, `--workspace`, `--profile <name>`,
    /// `-p`/`--package <name>` (repeatable), `--recipe-path <path>`,
    /// `--keep-recipe`, `--prepare-only`, `--cook-only`.
    Cook {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rustc from the active toolchain
    Rustc {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rustfmt from the active toolchain
    Rustfmt {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run clippy-driver from the active toolchain
    #[command(name = "clippy-driver")]
    ClippyDriver {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rustdoc from the active toolchain
    Rustdoc {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rust-gdb from the active toolchain
    #[command(name = "rust-gdb")]
    RustGdb {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rust-lldb from the active toolchain
    #[command(name = "rust-lldb")]
    RustLldb {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rust-analyzer from the active toolchain
    #[command(name = "rust-analyzer")]
    RustAnalyzer {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show cache status and tool info
    Status {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
    },
    /// Clear the managed zccache build cache
    Clean,
    /// Purge all soldr-managed cache artifacts
    Purge,
    /// Show or set configuration
    Config,
    /// Inspect the compilation cache
    Cache {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<CacheSubcommand>,
    },
    /// Show version
    Version {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
    },
    /// Review reclaimable Cargo `target/` directories tracked by
    /// the soldr registry (`~/.soldr/state.redb`).
    ///
    /// Aliases: `purge-targets` (matches issue #234's `soldr --purge`
    /// wording).
    #[command(alias = "purge-targets")]
    Gc {
        /// Deprecated: `soldr gc` is already a non-destructive summary.
        #[arg(long, hide = true)]
        dry_run: bool,
        /// Deprecated: use `soldr gc purge --all`.
        #[arg(long, hide = true)]
        all: bool,
        /// Minimum age before a `target/` is included in the summary
        /// (e.g. `10d`, `4w`).
        #[arg(long, default_value = "10d", value_name = "DURATION")]
        older_than: String,
        /// Minimum on-disk size before a `target/` is included in the
        /// summary (e.g. `256M`, `1GB`).
        #[arg(long, default_value = "256M", value_name = "SIZE")]
        larger_than: String,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<GcSubcommand>,
    },
    /// Drop-in passthrough to the system `rustup` binary.
    ///
    /// When the first non-flag positional argument is `target` or
    /// `component` and `rust-toolchain.toml` declares a `channel`,
    /// soldr automatically inserts `--toolchain <channel>` after the
    /// subcommand (unless the user already passed `--toolchain`).
    /// Every other invocation is forwarded verbatim.
    Rustup {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Read `rust-toolchain.toml` and install the declared channel /
    /// components / targets via `rustup`.
    Toolchain {
        #[command(subcommand)]
        subcommand: ToolchainSubcommand,
    },
    /// Install `rustup` itself into the soldr-managed bin dir when the
    /// host has no system-managed toolchain manager. Idempotent — a
    /// re-run with rustup already present prints the resolved path and
    /// exits 0. Fetches `rustup-init` from
    /// `https://static.rust-lang.org/rustup/dist/<host-triple>/` under
    /// the same `SOLDR_TRUST_MODE` / `SOLDR_CHECKSUMS_FILE` policy as
    /// every other soldr-fetched binary. Set `SOLDR_NO_BOOTSTRAP=1` to
    /// disable the implicit auto-install that runs from
    /// `soldr cargo` / `soldr rustup ...` when rustup is missing.
    Bootstrap {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Diagnose drift between `rust-toolchain.toml` and the
    /// currently installed rustup state. Read-only — never mutates
    /// rustup. Exit code is `1` when drift is detected, `0` otherwise.
    Doctor {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Force a fresh Defender real-time-scan probe of the soldr
        /// cache directory, ignoring the cached result. No-op outside
        /// Windows. Issue #357.
        #[arg(long)]
        refresh_defender_probe: bool,
    },
    /// Apply platform-specific hot-cache optimizations (Windows
    /// Defender exclusions today; future platforms TBD). Auto-skips on
    /// CI. See `docs/API.md` for the full matrix.
    Optimize(optimize::OptimizeArgs),
    /// Start a zccache session and return its identifier.
    ///
    /// Idempotent: when `ZCCACHE_SESSION_ID` is already set in the
    /// environment (and `--id` is not), emits the existing session
    /// metadata without contacting the daemon. Otherwise boots the
    /// daemon if necessary and runs `zccache session-start`.
    #[command(name = "session-start")]
    SessionStart {
        /// Explicit session id. Without this flag soldr lets zccache
        /// assign one.
        #[arg(long, value_name = "UUID")]
        id: Option<String>,
        /// Override the session log path. Defaults to the soldr-managed
        /// `<cache>/zccache/logs/last-session.log`.
        #[arg(long, value_name = "PATH")]
        log: Option<std::path::PathBuf>,
        /// Override the per-session JSONL journal path. Defaults to the
        /// soldr-managed `<cache>/zccache/logs/last-session.jsonl`.
        #[arg(long, value_name = "PATH")]
        journal: Option<std::path::PathBuf>,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// End a zccache session and emit its finalized stats.
    ///
    /// Idempotent: a second call against an already-finalized session
    /// reports the prior stats (or notes that the session is gone)
    /// without erroring.
    #[command(name = "session-end")]
    SessionEnd {
        /// Session id to end. Defaults to `$ZCCACHE_SESSION_ID`.
        #[arg(long, value_name = "UUID")]
        id: Option<String>,
        /// After ending the session, drop its journal/log files from
        /// disk so the next session-start begins from a clean slate.
        #[arg(long)]
        clear: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Install zccache binaries into soldr's private dir so soldr stops
    /// fetching the managed GitHub release. Pins a user-supplied set of
    /// three zccache binaries (zccache, zccache-daemon, zccache-fp) into
    /// `<SoldrPaths::bin>/zccache-pinned/`. Subsequent `soldr cargo ...`
    /// invocations resolve the pinned binaries automatically.
    ///
    /// Exactly one of `<source>`, `--remove`, or `--status` must be
    /// provided. `<source>` accepts `system`, a directory or archive
    /// path (`.zip` / `.tar.gz` / `.tar.zst`), or an `http(s)://` URL
    /// pointing at such an archive.
    #[command(name = "install-zccache", alias = "update-zccache")]
    InstallZccache {
        /// Source for the three zccache binaries. Mutually exclusive
        /// with `--remove` / `--status`.
        #[arg(value_name = "SOURCE", conflicts_with_all = ["remove", "status"])]
        source: Option<String>,
        /// Delete the pinned install and fall back to the managed
        /// fetch on next run. Idempotent.
        #[arg(long, conflicts_with_all = ["source", "status"])]
        remove: bool,
        /// Print the install dir, source, version, and per-binary
        /// sha256s of the pinned install (if any).
        #[arg(long, conflicts_with_all = ["source", "remove"])]
        status: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Bundle a build-cache directory plus a content-verified
    /// snapshot of source-file mtimes into a single `.tar.zst`
    /// archive. The output is consumed by `soldr load` to restore
    /// both the cache and Cargo-friendly source mtimes on a fresh
    /// checkout.
    Save(save_load::SaveArgs),
    /// Restore an archive produced by `soldr save`: unpack the cache
    /// to the destination directory and replay each source-file
    /// mtime, but only when the current file's size and BLAKE3 hash
    /// still match the snapshot (so we cannot underbuild after a
    /// real source change).
    Load(save_load::LoadArgs),
    /// Manage the long-lived `soldr-daemon` companion process that owns
    /// target/ tracking. Phase 1 — `start`, `stop`, `status` only.
    Daemon {
        #[command(subcommand)]
        command: DaemonSubcommand,
    },
    /// Anything else is a tool to fetch and run
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(clap::Subcommand)]
pub(crate) enum DaemonSubcommand {
    /// Start the soldr-daemon. With `--foreground`, runs in the current
    /// process (blocks until the daemon exits); without it, spawns the
    /// daemon detached and returns immediately.
    Start {
        #[arg(long)]
        foreground: bool,
        /// Seconds of inactivity after which the daemon auto-exits.
        #[arg(long, value_name = "SECS", default_value_t = 1800)]
        idle_timeout: u64,
    },
    /// Ask the running daemon to shut down gracefully.
    Stop,
    /// Print the daemon's status (uptime, pid, request count).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Query recorded build sessions.
    Builds {
        #[command(subcommand)]
        command: DaemonBuildsSubcommand,
    },
}

#[derive(clap::Subcommand)]
pub(crate) enum DaemonBuildsSubcommand {
    /// List recent build sessions, newest first.
    List {
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long, value_name = "UNIX_MS")]
        since_ms: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// List the slowest finished build sessions whose `total_wall_ms`
    /// meets the threshold (default 60s).
    Slow {
        #[arg(long, default_value_t = 60_000, value_name = "MS")]
        threshold_ms: u64,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Subcommand)]
pub(crate) enum ToolchainSubcommand {
    /// Install the channel declared in `rust-toolchain.toml`. No-op
    /// (exit 0 with a note) when the manifest is missing or omits
    /// `channel`.
    Install,
    /// Install the channel and every declared component / target from
    /// `rust-toolchain.toml`. Stops at the first nonzero rustup exit.
    Prepare,
}

#[derive(clap::Subcommand)]
pub(crate) enum GcSubcommand {
    /// Delete eligible Cargo `target/` directories.
    Purge {
        /// Delete every eligible candidate without prompting.
        #[arg(long)]
        all: bool,
        /// Minimum age before a `target/` is considered stale
        /// (e.g. `10d`, `4w`).
        #[arg(long, default_value = "10d", value_name = "DURATION")]
        older_than: String,
        /// Minimum on-disk size before a `target/` is considered for
        /// reclamation (e.g. `256M`, `1GB`).
        #[arg(long, default_value = "256M", value_name = "SIZE")]
        larger_than: String,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Narrow the purge to a single taxonomy kind. Mutually exclusive
        /// with `--registry-src` / `--git-checkouts`. Accepted values:
        /// `cargo_target`, `cargo_registry_src` (#323 slice 2),
        /// `cargo_git_checkouts` (#323 slice 3).
        #[arg(long, value_enum, conflicts_with_all = ["registry_src", "git_checkouts"])]
        kind: Option<GcListKind>,
        /// Shorthand for `--kind cargo_registry_src`. Walks
        /// `$CARGO_HOME/registry/src/<reg>/<crate>-<vers>/` and deletes
        /// the listed directories (#323 slice 2).
        #[arg(long, conflicts_with_all = ["kind", "git_checkouts"])]
        registry_src: bool,
        /// Shorthand for `--kind cargo_git_checkouts`. Walks
        /// `$CARGO_HOME/git/checkouts/<repo>/<commit>/` and deletes
        /// the listed directories (#323 slice 3).
        #[arg(long, conflicts_with_all = ["kind", "registry_src"])]
        git_checkouts: bool,
    },
    /// List every `target/` directory currently tracked in the soldr
    /// registry, without applying any age or size thresholds.
    List {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Narrow the listing to a single taxonomy kind (#323 slice 2).
        /// Accepted values: `cargo_target`, `cargo_registry_src`.
        #[arg(long, value_enum)]
        kind: Option<GcListKind>,
    },
    /// Run cargo's native `clean gc` against `$CARGO_HOME`. Requires
    /// a nightly toolchain because the command lives behind the
    /// unstable `-Zgc` flag.
    Cargo(Box<GcCargoArgs>),
    /// Read-only enumeration of every cache directory soldr knows
    /// about. Does not delete anything.
    Locations {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Orchestrator that runs `gc locations`, then conservative cargo
    /// `clean gc`, then the soldr target purge — and, with
    /// `--aggressive`, a second cargo GC pass with tighter ages.
    Sweep(Box<GcSweepArgs>),
}

/// Taxonomy kinds accepted by `gc list --kind` / `gc purge --kind`
/// (#323 slice 2). Unknown values are rejected at clap-parse time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum GcListKind {
    /// Workspace `target/` dirs tracked by the soldr registry.
    #[value(name = "cargo_target")]
    CargoTarget,
    /// `$CARGO_HOME/registry/src/<reg>/<crate>-<vers>/` extracted
    /// crate sources.
    #[value(name = "cargo_registry_src")]
    CargoRegistrySrc,
    /// `$CARGO_HOME/git/checkouts/<repo>/<commit>/` git-source crate
    /// checkouts (#323 slice 3).
    #[value(name = "cargo_git_checkouts")]
    CargoGitCheckouts,
}

#[derive(clap::Args)]
pub(crate) struct GcCargoArgs {
    /// Report the plan and exit without invoking cargo.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Override the nightly toolchain. Defaults to
    /// `$SOLDR_GC_CARGO_TOOLCHAIN` if set, else `nightly`.
    #[arg(long, value_name = "TOOLCHAIN")]
    pub(crate) toolchain: Option<String>,
    /// Forwarded directly to cargo `--max-src-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_src_age: Option<String>,
    /// Forwarded directly to cargo `--max-crate-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_crate_age: Option<String>,
    /// Forwarded directly to cargo `--max-index-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_index_age: Option<String>,
    /// Forwarded directly to cargo `--max-git-co-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_git_co_age: Option<String>,
    /// Forwarded directly to cargo `--max-git-db-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_git_db_age: Option<String>,
    /// Forwarded directly to cargo `--max-download-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_download_age: Option<String>,
    /// Forwarded directly to cargo `--max-src-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_src_size: Option<String>,
    /// Forwarded directly to cargo `--max-crate-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_crate_size: Option<String>,
    /// Forwarded directly to cargo `--max-git-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_git_size: Option<String>,
    /// Forwarded directly to cargo `--max-download-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_download_size: Option<String>,
    /// Emit the stable machine-facing JSON form for this command.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Args)]
pub(crate) struct GcSweepArgs {
    /// Delete every eligible target/ candidate without prompting (used
    /// when the orchestrator runs the soldr target purge stage).
    #[arg(long)]
    pub(crate) all: bool,
    /// Plan and report without deleting anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Run cargo's `clean gc`. Default is on; pass `--no-cargo-gc` to
    /// skip cargo entirely. `--cargo-gc` is accepted but is the
    /// default.
    #[arg(long, conflicts_with = "no_cargo_gc")]
    pub(crate) cargo_gc: bool,
    /// Skip cargo's `clean gc` (e.g. on CI runners with no nightly).
    #[arg(long, conflicts_with = "cargo_gc")]
    pub(crate) no_cargo_gc: bool,
    /// After the standard pipeline, run cargo's `clean gc` again with
    /// tighter ages
    /// (`--max-src-age=7days --max-crate-age=14days --max-git-co-age=7days`).
    /// Floor: each value is clamped to `auto_gc.min_age_secs` before
    /// being forwarded.
    #[arg(long)]
    pub(crate) aggressive: bool,
    /// Emit the stable machine-facing JSON form for this command.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Subcommand)]
pub(crate) enum CacheSubcommand {
    /// Roll up the most recent compile-cache session into an
    /// AI-readable diagnosis document. Reads
    /// `<zccache_dir>/logs/last-session-stats.json` (written by soldr
    /// at session-end) and, when available, calls `zccache analyze` for
    /// per-tool/per-extension breakdown over the per-session journal.
    Report {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Gracefully end the active session and stop the zccache daemon.
    ///
    /// Synchronous: does not return until the daemon process has
    /// exited, so the caller can safely snapshot the cache directory
    /// after this completes. Triggers the depgraph flush that landed in
    /// zccache 1.8.0.
    Shutdown {
        /// If set, copy the session log/journal/stats files into
        /// `<dir>/<session-id>/` before stopping the daemon. The
        /// directory (and any missing parents) is created on demand.
        #[arg(long, value_name = "DIR")]
        archive_logs: Option<std::path::PathBuf>,
        /// Skip the depgraph flush prior to stopping the daemon
        /// (debugging only; surface to skip the new 1.8.x persistence).
        #[arg(long)]
        no_depgraph_save: bool,
        /// Maximum seconds to wait for the daemon process to exit
        /// before returning a non-zero status.
        #[arg(long, value_name = "SECONDS", default_value_t = 30)]
        shutdown_timeout_seconds: u64,
        /// Skip the post-signal poll that confirms the daemon process
        /// has actually exited. By default `shutdown` blocks until
        /// `zccache status` reports the daemon is gone (or the
        /// `--shutdown-timeout-seconds` deadline elapses); pass
        /// `--no-wait` only when you genuinely do not care
        /// (interactive shells). See soldr#383.
        #[arg(long)]
        no_wait: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Synchronously serialize the in-memory depgraph (and any other
    /// in-memory zccache state) to disk without stopping the daemon.
    ///
    /// Returns 0 only after the bytes are durable (zccache fsync'd the
    /// snapshot). Pair with `cache shutdown` in CI post steps to
    /// guarantee the tar snapshot captures the freshest depgraph even
    /// when the daemon is later killed by an external signal. See
    /// soldr#383.
    Flush {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Prune stale per-prefix build artifacts from a cargo `target/`
    /// directory, keeping only the newest entry per
    /// `(parent_dir, prefix)` bucket inside
    /// `target/<profile>/{deps, .fingerprint, incremental, build}/`.
    ///
    /// Defaults to a dry run for safety. Pass `--force` (or
    /// `--no-dry-run`) to actually delete entries.
    #[command(name = "prune-target")]
    PruneTarget {
        /// Path to the cargo `target/` directory to prune.
        path: std::path::PathBuf,
        /// Explicit dry-run mode (this is the default). Accepted for
        /// scriptability; mutually compatible with the default.
        #[arg(long, conflicts_with_all = ["force", "no_dry_run"])]
        dry_run: bool,
        /// Negate the dry-run default and actually delete entries.
        /// Equivalent to `--force`.
        #[arg(long = "no-dry-run", conflicts_with = "dry_run")]
        no_dry_run: bool,
        /// Actually delete entries. Equivalent to `--no-dry-run`.
        #[arg(long, conflicts_with = "dry_run")]
        force: bool,
        /// Switch from the legacy per-`(parent_dir, prefix)` orphan
        /// prune (issue #336) to the aggressive per-`prefix` strategy
        /// (issue #316): keep only the **newest hash family** per
        /// logical artifact name, deleting every other hash's files
        /// across `deps/`, `.fingerprint/`, `incremental/`, and
        /// `build/`. Recency is ranked by cargo's authoritative
        /// `.fingerprint/<prefix>-<hash>/invoked.timestamp` mtime when
        /// available, falling back to the entry's own filesystem
        /// mtime.
        #[arg(long = "keep-latest")]
        keep_latest: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Trim cargo-recreatable noise from a `target/` directory before
    /// it is archived for CI cache transport. Composes:
    ///
    ///   * orphan hash-sibling pruning (same as `cache prune-target`),
    ///   * strip of build-script logspam, recreatable binaries, and
    ///     debug sidecars (CI profile only),
    ///   * removal of `target/<profile>/incremental/` (CI profile only).
    ///
    /// The CI profile is intended for `setup-soldr` to call before its
    /// `actions/cache@v4` save step so the rehydrate ships dramatically
    /// fewer bytes. Local profile only runs the hash-sibling prune.
    ///
    /// Dry-run by default. Pass `--force` to actually delete entries.
    /// Refuses to run when a `.cargo-lock` is present (active build).
    #[command(name = "trim-target")]
    TrimTarget {
        /// Path to the cargo `target/` directory to trim.
        path: std::path::PathBuf,
        /// Trim profile selector. `local` (default): only orphan
        /// hash-sibling prune. `ci`: also strip recreatable noise +
        /// remove incremental/.
        #[arg(long, value_enum, default_value_t = TrimProfileArg::Local)]
        profile: TrimProfileArg,
        /// Explicit dry-run mode (this is the default).
        #[arg(long, conflicts_with_all = ["force", "no_dry_run"])]
        dry_run: bool,
        /// Negate the dry-run default and actually delete entries.
        #[arg(long = "no-dry-run", conflicts_with = "dry_run")]
        no_dry_run: bool,
        /// Actually delete entries. Equivalent to `--no-dry-run`.
        #[arg(long, conflicts_with = "dry_run")]
        force: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
}

/// Trim profile presets for `cache trim-target`. Local keeps everything
/// a developer might want to inspect (incremental/, examples/, large
/// build-script stderr); CI strips it all in service of a smaller
/// archive.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum TrimProfileArg {
    /// Lightweight: only prune orphan hash siblings.
    Local,
    /// Aggressive: prune + strip recreatable noise + drop
    /// `incremental/`.
    Ci,
}
