// Mirror lib.rs: the bin tree compiles the same `fetch::*`,
// `cache_lib::*`, `core::*` modules independently of the lib tree.
// Items declared for external (lib) callers are dead from the bin's
// perspective and would trip `-D warnings` on CI. lib.rs has the same
// allow.
#![allow(dead_code, unused_imports)]

use clap::{Parser, Subcommand};

mod binaries;
mod bootstrap;
mod cache;
mod cache_lib;
mod cargo_diagnostics;
mod cargo_front_door;
mod cook;
mod core;
mod daemon;
mod defender_probe;
mod doctor;
mod fetch;
mod fuzzy_match;
mod gc;
mod linker;
mod native_cc;
mod optimize;
mod optimize_detect;
mod optimize_windows;
mod rust_plan;
mod save_load;
mod self_relocate;
mod startup_profile;
mod toolchain;
mod trampoline;
mod trampoline_workspace;
mod wrapper;
mod wrapper_target;
mod zccache;

use crate::core::{suppress_windows_console_window, SoldrError};
use crate::fetch::VersionSpec;

#[allow(unused_imports)]
pub(crate) use binaries::{
    apply_implicit_toolchain_homes, cached_active_zccache, current_soldr_binary,
    fetch_active_zccache, non_empty_env_path, parse_tool_spec, resolve_toolchain_binary,
    rustup_binary, rustup_resolution_failure, zccache_binary_override,
};

pub(crate) const TEST_CARGO_BIN_ENV_VAR: &str = "SOLDR_TEST_CARGO_BIN";
pub(crate) const TEST_RUSTC_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTC_BIN";
pub(crate) const TEST_RUSTUP_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTUP_BIN";
pub(crate) const TEST_ZCCACHE_BIN_ENV_VAR: &str = "SOLDR_TEST_ZCCACHE_BIN";
pub(crate) const TEST_FREE_DISK_BYTES_ENV_VAR: &str = "SOLDR_TEST_FREE_DISK_BYTES";
/// Overrides the default `nightly` toolchain used by `soldr gc cargo`
/// to invoke cargo's unstable `-Zgc clean gc`. The `--toolchain` flag
/// on `gc cargo` / `gc sweep` always takes precedence.
pub(crate) const SOLDR_GC_CARGO_TOOLCHAIN_ENV_VAR: &str = "SOLDR_GC_CARGO_TOOLCHAIN";
pub(crate) const JSON_SCHEMA_VERSION: u32 = 1;
pub(crate) const LOW_DISK_WARNING_THRESHOLD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(crate) const RUSTC_WRAPPER_OVERRIDE_ENV_VAR: &str = "SOLDR_RUSTC_WRAPPER";
pub(crate) const CARGO_PROFILE_DEV_DEBUG_ENV_VAR: &str = "CARGO_PROFILE_DEV_DEBUG";
pub(crate) const CARGO_PROFILE_TEST_DEBUG_ENV_VAR: &str = "CARGO_PROFILE_TEST_DEBUG";
/// Picks the linker injected for `soldr cargo ...` builds. See `linker` module
/// and `docs/API.md` for the supported values (`default | ld | mold | rust-lld
/// | fast`).
pub(crate) const LINKER_ENV_VAR: &str = "SOLDR_LINKER";
pub(crate) const REAL_TOOLCHAIN_BINARY_ENV_PREFIX: &str = "SOLDR_REAL_";
pub(crate) const TARGET_CACHE_MODE_ENV_VAR: &str = "SOLDR_TARGET_CACHE_MODE";
pub(crate) const TARGET_CACHE_BUNDLE_DIR_ENV_VAR: &str = "SOLDR_TARGET_CACHE_BUNDLE_DIR";
pub(crate) const TARGET_CACHE_BACKEND_ENV_VAR: &str = "SOLDR_TARGET_CACHE_BACKEND";
/// Selects which thin-slice pruning policy `soldr cargo` ships to zccache. See
/// `docs/THIN_TARGET_CACHE_PRUNING.md` for the rationale and rollout plan.
/// Values: `thin-v1` (legacy opt-out — keeps `.rlib`/`.rmeta`/proc-macro
/// outputs; useful when pinned to managed zccache < 1.9.1) and `thin-v2`
/// (default since soldr v0.7.31 / issue #461; fingerprint-aware aggressive
/// prune that drops library bytes and lets zccache's compilation cache
/// repopulate them).
pub(crate) const TARGET_CACHE_PROFILE_ENV_VAR: &str = "SOLDR_TARGET_CACHE_PROFILE";
/// Reader-thread count for the target-cache tar walk in zccache (issue #272).
/// Forwarded to the `zccache rust-plan save/restore` subprocess via inherited
/// environment; soldr validates the value early so typos fail before cargo
/// metadata runs. Values: `auto` (default; zccache picks a vCPU-bounded count
/// capped at 8), `1` (disable parallelism — sequential tar walk), or any
/// positive integer for an explicit thread count. The actual parallel walk
/// lives in zccache; this constant exists so soldr can reject malformed values
/// at the front door.
pub(crate) const TARGET_CACHE_TAR_THREADS_ENV_VAR: &str = "SOLDR_TARGET_CACHE_TAR_THREADS";
/// Filename of the file-list manifest written next to the thin-slice bundle so
/// downstream tooling can prove what landed in the slice without unpacking it.
pub(crate) const THIN_MANIFEST_FILENAME: &str = "manifest.v2.json";
/// Flag controlling the warm-restore short-circuit (issue #229). Default-on:
/// after a successful `rust-plan save` soldr writes a sentinel describing the
/// plan/job, and on the next `soldr cargo ...` invocation the matching
/// `rust-plan restore` is skipped if the sentinel proves the `target/` tree
/// is already in the exact state restore would produce. This preserves
/// Cargo's mtime-based fingerprints across split CI steps. Set to a falsy
/// value (`0` / `false` / `no` / `off` / empty, case-insensitive) to opt out;
/// unset or any other value keeps the short-circuit enabled.
pub(crate) const SKIP_WARM_RESTORE_ENV_VAR: &str = "SOLDR_RUST_PLAN_SKIP_WARM_RESTORE";
/// Filename of the sentinel written next to the thin-slice bundle root after
/// a successful `rust-plan save`. Read on the next invocation by
/// `should_skip_warm_restore` to decide whether `rust-plan restore` would be
/// a no-op-but-touches-mtimes operation against an already-warm `target/`.
pub(crate) const WARM_RESTORE_SENTINEL_FILENAME: &str = "last-save.json";
/// Maximum age of a warm-restore sentinel before it is treated as stale and
/// ignored. Five minutes comfortably covers a normal `cargo test --no-run`
/// followed by `cargo test` step pair on GitHub Actions while keeping the
/// short-circuit from kicking in on later, unrelated jobs.
pub(crate) const WARM_RESTORE_MAX_AGE_SECONDS: u64 = 5 * 60;

/// Pin a specific soldr version to handle this invocation. Explicit
/// `--as <version>` flag takes precedence over this env var.
const SOLDR_AS_ENV_VAR: &str = "SOLDR_AS";
/// Sentinel that the currently-running soldr was itself invoked by another
/// soldr through `--as`. Prevents infinite hand-offs.
const SOLDR_TRAMPOLINING_ENV_VAR: &str = "SOLDR_TRAMPOLINING";

#[derive(Parser)]
#[command(name = "soldr", version, about = "Instant tools. Instant builds.")]
struct Cli {
    /// Disable soldr's compilation cache for this invocation
    #[arg(long)]
    no_cache: bool,
    /// Pick the zccache binary backing the compilation cache.
    ///
    /// `managed` (default) fetches the pinned zccache release into
    /// `~/.soldr/`. `system` uses the `zccache` already on PATH
    /// (must have `zccache-daemon` and `zccache-fp` as siblings).
    #[arg(long, value_enum, default_value_t = ZccacheSourceArg::Managed, value_name = "SOURCE")]
    zccache: ZccacheSourceArg,
    #[command(subcommand)]
    command: Commands,
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
const SOLDR_BUILTIN_VERBS: &[&str] = &[
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

#[derive(Subcommand)]
enum Commands {
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

#[derive(Subcommand)]
enum DaemonSubcommand {
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

#[derive(Subcommand)]
enum DaemonBuildsSubcommand {
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

#[derive(Subcommand)]
enum ToolchainSubcommand {
    /// Install the channel declared in `rust-toolchain.toml`. No-op
    /// (exit 0 with a note) when the manifest is missing or omits
    /// `channel`.
    Install,
    /// Install the channel and every declared component / target from
    /// `rust-toolchain.toml`. Stops at the first nonzero rustup exit.
    Prepare,
}

#[derive(Subcommand)]
enum GcSubcommand {
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
        /// with `--registry-src`. Accepted values: `cargo_target`,
        /// `cargo_registry_src` (#323 slice 2).
        #[arg(long, value_enum, conflicts_with = "registry_src")]
        kind: Option<GcListKind>,
        /// Shorthand for `--kind cargo_registry_src`. Walks
        /// `$CARGO_HOME/registry/src/<reg>/<crate>-<vers>/` and deletes
        /// the listed directories (#323 slice 2).
        #[arg(long, conflicts_with = "kind")]
        registry_src: bool,
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

#[derive(Subcommand)]
enum CacheSubcommand {
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

#[tokio::main]
async fn main() {
    // RUSTC_WRAPPER mode: cargo passes `soldr /path/to/rustc <args...>`
    // Must be checked before clap parsing.
    let raw_args: Vec<String> = std::env::args().collect();
    if should_self_relocate_for_invocation(&raw_args) {
        match self_relocate::maybe_reexec_from_runtime(&raw_args) {
            Ok(Some(code)) => std::process::exit(code),
            Ok(None) => {}
            Err(error) => std::process::exit(report_and_exit(error)),
        }
    }

    if raw_args.len() > 1 && wrapper::is_wrapper_invocation(&raw_args[1]) {
        // Per-phase startup timing for #440. `WrapperProfile::new()` is a
        // cheap branch + one `var_os` syscall when SOLDR_PROFILE_STARTUP
        // is unset, so the dominant production path pays effectively
        // nothing. When set, the profile captures `Instant::now()` at
        // each boundary down to the exec call.
        let mut profile = startup_profile::WrapperProfile::new();
        profile.mark("args_collected");
        if let Some(version) = soldr_as_env_pin() {
            if should_trampoline(&version) {
                std::process::exit(
                    run_trampoline(&version, &raw_args[1..])
                        .await
                        .unwrap_or_else(report_and_exit),
                );
            }
        }
        profile.mark("pin_check_done");
        std::process::exit(
            wrapper::run_rustc_wrapper(&raw_args, profile).unwrap_or_else(report_and_exit),
        );
    }

    // `--as <version>` trampoline. Peeled off before clap so the fetched
    // older soldr parses its own argv on its own terms.
    let (pinned_version, trampoline_args) = match extract_as_pin(&raw_args[1..]) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("soldr: {e}");
            std::process::exit(1);
        }
    };
    let pinned_version = pinned_version.or_else(soldr_as_env_pin);

    if let Some(version) = pinned_version {
        if should_trampoline(&version) {
            std::process::exit(
                run_trampoline(&version, &trampoline_args)
                    .await
                    .unwrap_or_else(report_and_exit),
            );
        }
        // Short-circuit: requested version == current. Continue with args
        // that have `--as <ver>` stripped.
        std::process::exit(
            run_with_args(&raw_args[0], &trampoline_args)
                .await
                .unwrap_or_else(report_and_exit),
        );
    }

    let rc = run_with_args(&raw_args[0], &raw_args[1..])
        .await
        .unwrap_or_else(report_and_exit);
    std::process::exit(rc);
}

fn soldr_as_env_pin() -> Option<String> {
    std::env::var(SOLDR_AS_ENV_VAR)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

async fn run_with_args(prog: &str, args: &[String]) -> Result<i32, SoldrError> {
    let mut argv: Vec<String> = Vec::with_capacity(args.len() + 1);
    argv.push(prog.to_string());
    argv.extend(args.iter().cloned());
    // Use parse_from (not try_parse_from) so clap handles --help / --version /
    // usage errors with its built-in exit(0) / exit(2), matching the original
    // invocation path's UX exactly.
    let cli = Cli::parse_from(argv);
    run_cli(cli).await.map(|_| 0)
}

async fn run_cli(cli: Cli) -> Result<(), SoldrError> {
    let cache_enabled = !cli.no_cache;
    let zccache_source = cli.zccache;

    match cli.command {
        Commands::Cargo { args } => {
            std::process::exit(
                cargo_front_door::run_cargo_front_door(&args, cache_enabled, zccache_source)
                    .await?,
            );
        }
        Commands::Cook { args } => {
            std::process::exit(cook::run_cook(&args, cache_enabled, zccache_source).await?);
        }
        Commands::Rustc { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rustc", &args)?);
        }
        Commands::Rustfmt { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rustfmt", &args)?);
        }
        Commands::ClippyDriver { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough(
                "clippy-driver",
                &args,
            )?);
        }
        Commands::Rustdoc { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rustdoc", &args)?);
        }
        Commands::RustGdb { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rust-gdb", &args)?);
        }
        Commands::RustLldb { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rust-lldb", &args)?);
        }
        Commands::RustAnalyzer { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough(
                "rust-analyzer",
                &args,
            )?);
        }
        Commands::Rustup { args } => {
            std::process::exit(toolchain::run_rustup_passthrough(&args)?);
        }
        Commands::Toolchain { subcommand } => match subcommand {
            ToolchainSubcommand::Install => {
                std::process::exit(toolchain::run_toolchain_install()?);
            }
            ToolchainSubcommand::Prepare => {
                std::process::exit(toolchain::run_toolchain_prepare()?);
            }
        },
        Commands::Bootstrap { json } => {
            std::process::exit(bootstrap::run_bootstrap(json).await?);
        }
        Commands::Doctor {
            json,
            refresh_defender_probe,
        } => {
            std::process::exit(doctor::run_doctor(json, refresh_defender_probe)?);
        }
        Commands::Optimize(args) => {
            std::process::exit(optimize::run_optimize(args)?);
        }
        Commands::InstallZccache {
            source,
            remove,
            status,
            json,
        } => {
            cache::run_install_zccache(source, remove, status, json).await?;
        }
        Commands::Save(args) => {
            std::process::exit(save_load::run_save(args));
        }
        Commands::Load(args) => {
            std::process::exit(save_load::run_load(args));
        }
        Commands::Status { json } => {
            let output = cache::collect_status_output(cache_enabled)?;
            if json {
                cache::print_json(&output)?;
            } else {
                cache::print_status_output(&output);
            }
        }
        Commands::Clean => {
            cache::clear_zccache_cache()?;
        }
        Commands::Purge => {
            cache::purge_soldr_cache()?;
        }
        Commands::Config => {
            println!("(config not yet implemented)");
        }
        Commands::Cache { json, command } => match command {
            Some(CacheSubcommand::Report { json: report_json }) => {
                cache::run_cache_report_command(report_json || json)?;
            }
            Some(CacheSubcommand::Shutdown {
                archive_logs,
                no_depgraph_save,
                shutdown_timeout_seconds,
                no_wait,
                json: shutdown_json,
            }) => {
                cache::run_cache_shutdown_command(
                    archive_logs,
                    no_depgraph_save,
                    shutdown_timeout_seconds,
                    !no_wait,
                    shutdown_json || json,
                )
                .await?;
            }
            Some(CacheSubcommand::Flush { json: flush_json }) => {
                cache::run_cache_flush_command(flush_json || json).await?;
            }
            Some(CacheSubcommand::PruneTarget {
                path,
                dry_run,
                no_dry_run,
                force,
                keep_latest,
                json: prune_json,
            }) => {
                let effective_dry_run = !(force || no_dry_run);
                // Either flag pair maps onto the same boolean; `dry_run`
                // is the documented default so we accept it explicitly.
                let _ = dry_run;
                cache::run_cache_prune_target_command(
                    path,
                    effective_dry_run,
                    keep_latest,
                    prune_json || json,
                )?;
            }
            Some(CacheSubcommand::TrimTarget {
                path,
                profile,
                dry_run,
                no_dry_run,
                force,
                json: trim_json,
            }) => {
                let effective_dry_run = !(force || no_dry_run);
                let _ = dry_run;
                let trim_profile = match profile {
                    TrimProfileArg::Local => cache::TrimProfile::Local,
                    TrimProfileArg::Ci => cache::TrimProfile::Ci,
                };
                cache::run_cache_trim_target_command(
                    path,
                    trim_profile,
                    effective_dry_run,
                    trim_json || json,
                )?;
            }
            None => {
                let output = cache::collect_cache_output()?;
                if json {
                    cache::print_json(&output)?;
                } else {
                    cache::print_cache_output(&output);
                }
            }
        },
        Commands::Version { json } => {
            let output = cache::version_output();
            if json {
                cache::print_json(&output)?;
            } else {
                println!("soldr {}", output.soldr_version);
            }
        }
        Commands::Gc {
            dry_run,
            all,
            older_than,
            larger_than,
            json,
            command,
        } => {
            if all {
                return Err(SoldrError::Other(
                    "`soldr gc --all` no longer deletes targets; use `soldr gc purge --all`".into(),
                ));
            }
            if dry_run && command.is_some() {
                return Err(SoldrError::Other(
                    "`soldr gc --dry-run` is a summary alias; use `soldr gc` or `soldr gc purge`"
                        .into(),
                ));
            }
            let invocation = match command {
                Some(GcSubcommand::Purge {
                    all,
                    older_than,
                    larger_than,
                    json,
                    kind,
                    registry_src,
                }) => {
                    // #323 slice 2: --registry-src is a shorthand for
                    // --kind cargo_registry_src; clap already enforces
                    // mutual exclusion.
                    let effective_kind = if registry_src {
                        Some(GcListKind::CargoRegistrySrc)
                    } else {
                        kind
                    };
                    match effective_kind {
                        Some(GcListKind::CargoRegistrySrc) => {
                            gc::run_gc_purge_registry_src_command(all, json)?;
                            return Ok(());
                        }
                        Some(GcListKind::CargoTarget) | None => gc::GcInvocation {
                            mode: gc::GcMode::Purge { all },
                            older_than,
                            larger_than,
                            json,
                        },
                    }
                }
                Some(GcSubcommand::List { json, kind }) => {
                    gc::run_gc_list_command(json, kind.map(Into::into))?;
                    return Ok(());
                }
                Some(GcSubcommand::Cargo(args)) => {
                    gc::run_gc_cargo_command(*args)?;
                    return Ok(());
                }
                Some(GcSubcommand::Locations { json }) => {
                    gc::run_gc_locations_command(json)?;
                    return Ok(());
                }
                Some(GcSubcommand::Sweep(args)) => {
                    gc::run_gc_sweep_command(*args)?;
                    return Ok(());
                }
                None => gc::GcInvocation {
                    mode: gc::GcMode::Summary,
                    older_than,
                    larger_than,
                    json,
                },
            };
            gc::run_gc_command(invocation)?;
        }
        Commands::SessionStart {
            id,
            log,
            journal,
            json,
        } => {
            cache::run_session_start_command(id, log, journal, json).await?;
        }
        Commands::SessionEnd { id, clear, json } => {
            cache::run_session_end_command(id, clear, json)?;
        }
        Commands::Daemon { command } => {
            run_daemon_command(command)?;
        }
        Commands::External(args) => {
            if args.is_empty() {
                eprintln!("usage: soldr <tool>[@version] [args...]");
                std::process::exit(1);
            }

            let (crate_name, version) = parse_tool_spec(&args[0]);
            let tool_args = &args[1..];

            // Issue #412: when the user typed a verb that LOOKS like
            // a typo or a renamed built-in (e.g. `update-zccacheee`,
            // `installzccache`), emit a "did you mean?" hint before
            // we fire the network fetch. The fetch still runs — the
            // suggestion is advisory.
            if let Some(suggestion) =
                fuzzy_match::suggest_close_match(&crate_name, SOLDR_BUILTIN_VERBS)
            {
                eprintln!("soldr: '{crate_name}' is not a known built-in soldr verb.");
                eprintln!("soldr: did you mean: {suggestion}?");
            }

            eprintln!("soldr: fetching {crate_name}...");
            let result = crate::fetch::fetch_tool(&crate_name, &version).await?;

            if result.cached {
                eprintln!("soldr: using cached {crate_name} v{}", result.version);
            } else {
                eprintln!("soldr: downloaded {crate_name} v{}", result.version);
            }

            let mut command = std::process::Command::new(&result.binary_path);
            command.args(tool_args);
            suppress_windows_console_window(&mut command);
            let status = command.status()?;

            std::process::exit(status.code().unwrap_or(1));
        }
    }

    Ok(())
}

fn report_and_exit(error: SoldrError) -> i32 {
    eprintln!("soldr: {error}");
    1
}

fn should_self_relocate_for_invocation(raw_args: &[String]) -> bool {
    let user_args = raw_args.get(1..).unwrap_or(&[]);
    let Ok((_, args)) = extract_as_pin(user_args) else {
        return false;
    };

    let mut cache_enabled = true;
    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--no-cache" => {
                cache_enabled = false;
                idx += 1;
            }
            "--zccache" => {
                // Value lives in the next token; skip both so the
                // subcommand check below lands on `cargo` instead of
                // the value.
                idx += 2;
            }
            "--" => return false,
            arg if arg.starts_with('-') => idx += 1,
            "cargo" => {
                return cache_enabled
                    && cargo_front_door::cargo_args_are_cacheable(&args[idx + 1..])
                    && matches!(
                        zccache::rustc_wrapper_mode(),
                        zccache::RustcWrapperMode::ManagedZccache
                    );
            }
            _ => return false,
        }
    }

    false
}

/// Extract `--as <version>` or `--as=<version>` from the leading flag
/// region of the user's argv. Stops scanning at the first non-flag
/// positional (conventionally the subcommand), so a `--as` appearing
/// after `cargo` belongs to cargo and is left alone.
fn extract_as_pin(args: &[String]) -> Result<(Option<String>, Vec<String>), SoldrError> {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut version: Option<String> = None;
    let mut before_subcommand = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !before_subcommand {
            out.push(arg.clone());
            continue;
        }
        if arg == "--as" {
            let value = iter.next().ok_or_else(|| {
                SoldrError::Other("--as requires a version argument, e.g. --as 0.5.2".into())
            })?;
            if version.is_some() {
                return Err(SoldrError::Other("--as specified more than once".into()));
            }
            if value.is_empty() {
                return Err(SoldrError::Other(
                    "--as version argument must not be empty".into(),
                ));
            }
            version = Some(value.clone());
            continue;
        }
        if let Some(value) = arg.strip_prefix("--as=") {
            if version.is_some() {
                return Err(SoldrError::Other("--as specified more than once".into()));
            }
            if value.is_empty() {
                return Err(SoldrError::Other(
                    "--as= requires a version, e.g. --as=0.5.2".into(),
                ));
            }
            version = Some(value.to_string());
            continue;
        }
        if arg == "--" {
            before_subcommand = false;
            out.push(arg.clone());
            continue;
        }
        if arg.starts_with('-') {
            out.push(arg.clone());
            continue;
        }
        before_subcommand = false;
        out.push(arg.clone());
    }
    Ok((version, out))
}

/// True when the requested version is different from this binary's. A match
/// short-circuits the trampoline so the current in-process soldr handles it.
fn should_trampoline(requested: &str) -> bool {
    let current = env!("CARGO_PKG_VERSION");
    normalize_version(requested) != normalize_version(current)
}

fn normalize_version(v: &str) -> String {
    v.trim().trim_start_matches('v').to_string()
}

async fn run_trampoline(version: &str, args: &[String]) -> Result<i32, SoldrError> {
    if let Ok(prior) = std::env::var(SOLDR_TRAMPOLINING_ENV_VAR) {
        return Err(SoldrError::Other(format!(
            "refusing to trampoline again: this process was already reached via `--as` from soldr {prior}. Drop the inner --as flag."
        )));
    }

    eprintln!("soldr: trampolining to soldr@{version}...");
    let result =
        crate::fetch::fetch_tool("soldr", &VersionSpec::Exact(normalize_version(version))).await?;

    if result.cached {
        eprintln!(
            "soldr: using cached soldr v{} at {}",
            result.version,
            result.binary_path.display()
        );
    } else {
        eprintln!(
            "soldr: downloaded soldr v{} to {}",
            result.version,
            result.binary_path.display()
        );
    }

    let mut command = std::process::Command::new(&result.binary_path);
    command
        .args(args)
        .env(SOLDR_TRAMPOLINING_ENV_VAR, env!("CARGO_PKG_VERSION"));
    suppress_windows_console_window(&mut command);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let err = command.exec();
        Err(SoldrError::Other(format!(
            "failed to exec soldr v{} at {}: {err}",
            result.version,
            result.binary_path.display()
        )))
    }

    #[cfg(not(unix))]
    {
        let status = command.status().map_err(|e| {
            SoldrError::Other(format!(
                "failed to exec soldr v{} at {}: {e}",
                result.version,
                result.binary_path.display()
            ))
        })?;

        Ok(status.code().unwrap_or(1))
    }
}

fn render_builds(
    result: Result<Vec<crate::daemon::protocol::BuildRecord>, crate::daemon::client::ClientError>,
    json: bool,
) -> Result<(), SoldrError> {
    use crate::daemon::client::ClientError;
    match result {
        Ok(rows) => {
            if json {
                let payload = serde_json::json!({
                    "builds": rows.iter().map(|r| serde_json::json!({
                        "session_id": r.session_id,
                        "repo_root": r.repo_root,
                        "started_at_ms": r.started_at_ms,
                        "ended_at_ms": r.ended_at_ms,
                        "exit_code": r.exit_code,
                        "total_wall_ms": r.total_wall_ms,
                        "crate_count": r.crate_count,
                        "slowest_crate_us": r.slowest_crate_us,
                        "slowest_crate_name": r.slowest_crate_name,
                    })).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string(&payload).unwrap_or_default());
            } else if rows.is_empty() {
                println!("(no recorded builds)");
            } else {
                for r in rows {
                    let wall = r
                        .total_wall_ms
                        .map(|m| format!("{m}ms"))
                        .unwrap_or_else(|| "running".into());
                    let exit = r
                        .exit_code
                        .map(|c| format!("exit={c}"))
                        .unwrap_or_else(|| "exit=?".into());
                    let slowest = r.slowest_crate_name.as_deref().unwrap_or("(none)");
                    println!(
                        "session_id={} repo={} wall={} {} crates={} slowest={}",
                        r.session_id, r.repo_root, wall, exit, r.crate_count, slowest
                    );
                }
            }
            Ok(())
        }
        Err(ClientError::NotRunning) => {
            if json {
                println!("{}", serde_json::json!({"running": false, "builds": []}));
            } else {
                println!("soldr-daemon: not running");
            }
            Ok(())
        }
        Err(e) => Err(SoldrError::Other(format!("daemon builds failed: {e:?}"))),
    }
}

fn run_daemon_command(command: DaemonSubcommand) -> Result<(), SoldrError> {
    use crate::daemon::client;
    use crate::daemon::lifecycle::{is_live, try_spawn_detached};
    use crate::daemon::server::{run as run_server, server_sock_path, ServerOptions};
    use core::SoldrPaths;
    use std::time::Duration;

    let paths = SoldrPaths::new()?;
    let sock = server_sock_path(&paths);

    match command {
        DaemonSubcommand::Start {
            foreground,
            idle_timeout,
        } => {
            if foreground {
                let opts = ServerOptions {
                    idle_timeout: if idle_timeout == 0 {
                        Duration::from_secs(u64::MAX / 2)
                    } else {
                        Duration::from_secs(idle_timeout)
                    },
                };
                run_server(opts)
                    .map_err(|e| SoldrError::Other(format!("soldr-daemon failed: {e:?}")))?;
                Ok(())
            } else {
                if is_live(&paths).is_some() {
                    println!("soldr-daemon already running");
                    return Ok(());
                }
                try_spawn_detached().map_err(|e| {
                    SoldrError::Other(format!("failed to spawn soldr-daemon: {e:?}"))
                })?;
                println!("soldr-daemon: spawn requested");
                Ok(())
            }
        }
        DaemonSubcommand::Stop => match client::shutdown(&sock) {
            Ok(()) => {
                println!("soldr-daemon: shutdown requested");
                Ok(())
            }
            Err(client::ClientError::NotRunning) => {
                println!("soldr-daemon: not running");
                Ok(())
            }
            Err(e) => Err(SoldrError::Other(format!("daemon stop failed: {e:?}"))),
        },
        DaemonSubcommand::Status { json } => match client::status(&sock) {
            Ok(info) => {
                if json {
                    let payload = serde_json::json!({
                        "running": true,
                        "version": info.version,
                        "pid": info.pid,
                        "uptime_secs": info.uptime_secs,
                        "request_count": info.request_count,
                        "linked_zccache_pid": info.linked_zccache_pid,
                    });
                    println!("{}", serde_json::to_string(&payload).unwrap_or_default());
                } else {
                    println!(
                        "soldr-daemon: pid={} uptime={}s requests={} version={}",
                        info.pid, info.uptime_secs, info.request_count, info.version
                    );
                }
                Ok(())
            }
            Err(client::ClientError::NotRunning) => {
                if json {
                    println!("{}", serde_json::json!({"running": false}));
                } else {
                    println!("soldr-daemon: not running");
                }
                Ok(())
            }
            Err(e) => Err(SoldrError::Other(format!("daemon status failed: {e:?}"))),
        },
        DaemonSubcommand::Builds { command } => match command {
            DaemonBuildsSubcommand::List {
                limit,
                since_ms,
                json,
            } => render_builds(client::list_builds(&sock, limit, since_ms), json),
            DaemonBuildsSubcommand::Slow {
                threshold_ms,
                limit,
                json,
            } => render_builds(client::list_slow_builds(&sock, threshold_ms, limit), json),
        },
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
