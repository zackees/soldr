//! clap derives for the `soldr` binary (top-level `Cli` + every
//! subcommand enum / args struct). Extracted from `main.rs` to keep
//! that file under the soldr-wide 1 000-LOC source budget enforced by
//! `ci/hooks/loc_guard.py` (warn >1K, block >1.5K). The split is
//! purely organisational — `main.rs` re-imports each type with
//! `use cli_args::*` so the dispatch path is unchanged.

use crate::{optimize, save_load};

const ROOT_LONG_ABOUT: &str = "Instant tools. Instant builds.\n\n\
soldr wraps cargo and the rustup toolchain so every Rust command goes through a\n\
managed, content-addressable cache and a repo-pinned toolchain.";

const ROOT_AFTER_HELP: &str = "\
Examples:\n  \
soldr cargo build --release   # cached build, pinned toolchain\n  \
soldr rustfmt --check src     # format check via pinned rustfmt\n  \
soldr toolchain               # install rust-toolchain.toml\n  \
soldr cook --release          # prebuild deps as a cache layer\n  \
soldr status                  # cache health + active toolchain\n\n\
Run `soldr help <command>` for detailed help on a subcommand,\n\
including environment variables, exit codes, and on-disk layout.";

const ROOT_BEFORE_HELP: &str = "\
Common commands:\n  \
cargo                  Run cargo through soldr (cached, pinned toolchain)\n  \
rustc                  Compile Rust source via the pinned toolchain\n  \
lint                   Run Soldr's unified Rust and dependency lint suites\n  \
rustfmt                Format Rust source via the pinned toolchain\n  \
clippy-driver          Run the clippy linter via the pinned toolchain\n  \
rustup                 Drop-in passthrough to the system rustup binary\n\n\
Less common toolchain commands:\n  \
rustdoc                Generate Rust documentation\n  \
rust-analyzer          Run the rust-analyzer language server\n  \
rust-gdb               Debug with gdb + Rust pretty-printers\n  \
rust-lldb              Debug with lldb + Rust pretty-printers\n  \
toolchain              Install the channel declared in rust-toolchain.toml\n  \
bootstrap              Install rustup itself into soldr's managed bin dir\n  \
doctor                 Report drift between rust-toolchain.toml and rustup\n  \
shims                  Install per-version PATH shims for Rust tools\n\n\
soldr cache & build:\n  \
cook                   Prebuild dependencies via bundled cargo-chef\n  \
status                 Show cache status and active toolchain\n  \
cache                  Inspect compilation cache entries\n  \
config                 Show or set soldr configuration\n  \
clean                  Clear the embedded zccache build cache\n  \
purge                  Purge all soldr-managed cache artifacts\n  \
gc                     Review reclaimable cargo target/ directories\n  \
save                   Bundle a build cache + source mtimes into a .tar.zst\n  \
load                   Restore a soldr-save archive on a fresh checkout\n\n\
soldr ops & infrastructure:\n  \
daemon                 Manage the long-lived soldr-daemon process\n  \
session-start          Start a zccache session and print its id\n  \
session-end            End a zccache session and emit its stats\n  \
optimize               Apply platform-specific hot-cache tuning\n  \
defender-exclusions    Manage Windows Defender exclusions for soldr caches\n\n  \
help                   Print this message or the help of the given subcommand\n\n";

#[derive(clap::Parser)]
#[command(
    name = "soldr",
    version,
    about = "Instant tools. Instant builds.",
    long_about = ROOT_LONG_ABOUT,
    before_help = ROOT_BEFORE_HELP,
    after_help = ROOT_AFTER_HELP,
    after_long_help = ROOT_AFTER_HELP,
    next_line_help = true,
    help_template = "{about-with-newline}\n{usage-heading} {usage}\n\n{before-help}Options:\n{options}{after-help}",
    max_term_width = 80
)]
pub(crate) struct Cli {
    #[arg(
        long,
        help = "Disable soldr's compilation cache for this run",
        long_help = "Disable soldr's compilation cache for this run (bypasses the \
wrapper + daemon; also the recovery path if a build hangs on a wedged cache). \
A truthy `ZCCACHE_DISABLE` env var is equivalent."
    )]
    pub(crate) no_cache: bool,
    /// Trust inherited soldr/zccache workspace environment for cargo runs
    #[arg(
        long,
        help = "Trust inherited soldr/zccache workspace env for cargo runs",
        long_help = "Trust inherited soldr/zccache workspace environment for cargo runs.\n\n\
By default, `soldr cargo ...` resolves a fresh soldr workspace context from the current cwd/manifest while preserving normal OS, Cargo, Rust, proxy, cert, and CI environment. This flag is an advanced escape hatch for CI/action workflows that intentionally inject soldr/zccache workspace state."
    )]
    pub(crate) trust_inherited_soldr_env: bool,
    #[arg(
        long,
        global = true,
        help = "Allow a build with no rust-toolchain.toml (soldr#1766)",
        long_help = "Proceed even when no rust-toolchain.toml exists at or above the working directory.

Without a pin soldr resolves rustc from PATH, which can select a mismatched-host toolchain and makes cache keys depend on ambient PATH state. This flag makes that degraded mode an explicit choice. Equivalent to SOLDR_ALLOW_UNPINNED=1."
    )]
    pub(crate) allow_unpinned: bool,
    /// soldr#1802 — force the elapsed-seconds line prefix on.
    ///
    /// A pair of plain boolean flags rather than one `--timestamp-lines=BOOL`.
    /// The default is *conditional* (on for non-TTY, off for a terminal), so
    /// "not passed" must stay distinguishable from "passed false" — but the
    /// `Option<bool>` spelling that expresses that (`num_args = 0..=1` +
    /// `default_missing_value`) sends clap into unbounded recursion when the
    /// arg is also `global = true`, overflowing the stack on *every*
    /// invocation including `--version`. Two flags express the same tri-state
    /// with no exotic clap features.
    #[arg(
        long,
        conflicts_with = "no_timestamp_lines",
        help = "Prefix relayed output lines with elapsed seconds (soldr#1802)",
        long_help = "Prefix every relayed output line with seconds elapsed since soldr started.

Equivalent to SOLDR_TIMESTAMP_LINES=1. Left unset, the prefix is on when stderr is not a terminal (CI, Docker, `2>file`) and off on an interactive terminal, where it would fight cargo's progress redraw.

Place it BEFORE `cargo`, as in `soldr --timestamp-lines cargo build`: everything after `cargo` is passed through to cargo untouched."
    )]
    pub(crate) timestamp_lines: bool,
    /// soldr#1802 — force the elapsed-seconds line prefix off.
    #[arg(
        long,
        help = "Suppress the elapsed-seconds line prefix (soldr#1802)",
        long_help = "Suppress the elapsed-seconds prefix on relayed output lines.

Equivalent to SOLDR_TIMESTAMP_LINES=0. Useful in CI, where the prefix is on by default, when a downstream tool parses cargo's exact bytes. Note the capture channel soldr's own diagnostic scanner reads is always raw, so this only affects what reaches your terminal.

Place it BEFORE `cargo`, as with --timestamp-lines."
    )]
    pub(crate) no_timestamp_lines: bool,
    #[arg(
        long,
        value_enum,
        default_value_t = ZccacheSourceArg::Managed,
        value_name = "SOURCE",
        hide_possible_values = true,
        help = "Embedded zccache compatibility selector",
        long_help = "Select the zccache integration backing the compilation cache.\n\n\
`managed` (default) uses the zccache service compiled into soldr-daemon.\n\
`system` is retained as a compatibility spelling and currently selects the\n\
same embedded service. To use an external cache wrapper, set\n\
`SOLDR_RUSTC_WRAPPER=/path/to/zccache`."
    )]
    pub(crate) zccache: ZccacheSourceArg,
    #[arg(
        long,
        global = true,
        value_name = "N",
        help = "Cap concurrent daemon compiles (soldr#1761)",
        long_help = "Cap the number of compiles the daemon runs concurrently.

Equivalent to SOLDR_JOBS=N, and takes the same top precedence: above `[jobs].max_parallel_compiles` in config.toml, and above the legacy ZCCACHE_MAX_PARALLEL_COMPILES. Left unset, the default leaves one logical CPU free, and on an SMT machine additionally caps at (physical cores + 2) so a build cannot saturate every hardware thread — 10 on an 8-core/16-thread host. A machine without SMT keeps all its slots, and if the CPU topology cannot be read the limit falls back to one less than the logical CPU count.

This governs the daemon, not cargo — it is not forwarded as cargo's own `-j`.

Applies to a daemon this invocation starts. A daemon already running keeps the limit it started with, so run `soldr daemon stop` first to change it."
    )]
    pub(crate) jobs: Option<usize>,
    #[command(subcommand)]
    pub(crate) command: Commands,
}

impl Cli {
    /// Publish the global flags that have an environment-variable spelling.
    ///
    /// Both of these must reach the daemon, which resolves them in its own
    /// process, and `SOLDR_*` is what survives the spawn scrub (soldr#1931).
    /// Setting the variable rather than threading a boolean keeps a single
    /// resolution point for each: `crate::toolchain` for the pin, and
    /// `core::jobs` for the compile limit — the flag simply populates that
    /// resolver's top precedence tier rather than becoming a second one.
    ///
    /// Lives here rather than in `run_cli` so the flag and its effect stay in
    /// one file; the dispatch path just calls this once.
    pub(crate) fn export_global_env(&self) {
        // soldr#1766.
        if self.allow_unpinned {
            std::env::set_var(crate::toolchain::ALLOW_UNPINNED_ENV_VAR, "1");
        }
        // soldr#1761.
        if let Some(jobs) = self.jobs {
            std::env::set_var(soldr_core::core::jobs::SOLDR_JOBS_ENV_VAR, jobs.to_string());
        }
        // soldr#1802. Publishing the variable rather than threading a bool
        // keeps `should_timestamp` the single decision point, so the flag
        // populates its top precedence tier instead of becoming a second
        // one that could disagree with it.
        // `conflicts_with` makes both-at-once a parse error, so at most one
        // arm runs and neither needs to defer to the other.
        if self.timestamp_lines || self.no_timestamp_lines {
            std::env::set_var(
                crate::cargo_front_door::timestamp_tee::TIMESTAMP_LINES_ENV_VAR,
                if self.timestamp_lines { "1" } else { "0" },
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ZccacheSourceArg {
    /// Use the zccache service embedded in soldr-daemon (default).
    #[default]
    Managed,
    /// Compatibility spelling; currently equivalent to `managed`.
    System,
}

/// Flat list of every built-in soldr verb that clap recognizes,
/// PLUS aliases (for example, `purge-targets` for `gc`). Used by the
/// fuzzy-match suggestion
/// path in `Commands::External` (issue #412) to detect typos /
/// pre-rename verbs that fell through to the external-tool fetch.
///
/// Must stay in sync with the `Commands` enum + `#[command(alias = ...)]`
/// attributes below. A unit test in `main_tests.rs` walks the const
/// against clap's discovered subcommands and fails when they drift,
/// so adding a new verb here without updating the enum (or vice
/// versa) trips the build.
/// Cargo's own first-party verbs that — when typed bare as
/// `soldr <verb>` — should be routed through soldr's cargo front
/// door (`soldr cargo <verb> ...`) instead of falling through
/// `Commands::External` and attempting a doomed crates.io fetch
/// for a literally-named crate (`build`, `test`, etc. are not
/// real crates). Issue #685, phase 2 of #682.
///
/// The collision verbs `clean`, `config`, and `version` are
/// deliberately EXCLUDED — those map to soldr-native built-ins
/// that clap captures before the External arm runs. `soldr cargo
/// clean` / `soldr cargo config` continue to work as the explicit
/// escape hatch. A unit test in `main_tests.rs` asserts the
/// exclusion stays in place.
///
/// The list is intentionally a superset of cargo's own `--list`
/// output of first-party commands; new cargo verbs (rare) need an
/// explicit add here.
pub(crate) const CARGO_BUILTIN_VERBS: &[&str] = &[
    // `build` is intentionally NOT here. As of soldr#1012 PR 1, `build`
    // is a soldr-native verb (`Commands::Build`) — clap captures it
    // before the External arm runs. It joins `clean`, `config`, and
    // `version` as a collision verb whose meaning is owned by soldr.
    // `soldr cargo build` is the explicit legacy-passthrough escape
    // hatch; `soldr build` is the blessed default.
    "test",
    "check",
    "run",
    "bench",
    "doc",
    "fmt",
    "clippy",
    "tree",
    "update",
    "fix",
    "add",
    "remove",
    "metadata",
    "pkgid",
    "search",
    "vendor",
    "yank",
    "owner",
    "login",
    "logout",
    "init",
    "new",
    "generate-lockfile",
    "verify-project",
    "locate-project",
    "report",
    "install",
    "uninstall",
    "publish",
];

/// Predicate form of [`CARGO_BUILTIN_VERBS`]. Lives next to the const
/// so callers (the External arm dispatcher and the tests) share one
/// source of truth.
pub(crate) fn is_cargo_builtin_verb(verb: &str) -> bool {
    CARGO_BUILTIN_VERBS.contains(&verb)
}

pub(crate) const SOLDR_BUILTIN_VERBS: &[&str] = &[
    // soldr#1012: `build` is a soldr-native verb (the blessed-default
    // surface). It layers catalogue-driven sysroot prep on top of the
    // cargo front door and stays paired with `Commands::Build` in the
    // enum.
    "build",
    // soldr#2139 gap 1 — the blessed abi3 Python wheel surface.
    "wheel",
    "cargo",
    "dylint",
    "cook",
    "lint",
    // soldr#1059 — PATH-prepending escape hatch for cargo extensions.
    "exec",
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
    // soldr#820 phase 1 — `soldr logs` discoverable runtime-log API
    "logs",
    "version",
    "gc",
    "purge-targets", // alias of `gc`
    "rustup",
    "toolchain",
    "bootstrap",
    "doctor",
    "shims",
    "optimize",
    "defender-exclusions",
    // pre-existing drift caught by the SOLDR_BUILTIN_VERBS gate while
    // landing soldr#1012 PR 1 — `Commands::Env` was added but never
    // registered in this const. Belongs here next to other verbs.
    "env",
    "session-start",
    "session-end",
    "save",
    "load",
    "archive",
    "prepare",
    "build-from-source",
    "daemon",
    "shims",
];

#[derive(clap::Subcommand)]
pub(crate) enum Commands {
    /// Build the workspace via soldr's blessed cross-compile path
    ///
    /// `soldr build --target X` is the **blessed-default surface** for
    /// builds. It forwards to the same `cargo_front_door` pipeline as
    /// `soldr cargo build`, but first prepares any supported target
    /// sysroot and compiler/linker environment. For example,
    /// `*-pc-windows-msvc` can use the managed xwin-cache with clang/lld
    /// directly, without routing the default path through `cargo xwin`.
    ///
    /// The surface contract is the load-bearing part: callers asking
    /// for `soldr build` get the soldr-blessed toolchain story, while
    /// callers asking for `soldr cargo build` get the explicit legacy
    /// passthrough.
    ///
    /// Internal sharing of dispatch with `Commands::Cargo` is
    /// intentional and expected; what matters is the user-facing
    /// contract that `soldr build` evolves into the blessed-default
    /// without breaking the alias surface.
    Build {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Build an abi3 Python wheel through soldr's blessed toolchain (soldr#2139)
    ///
    /// `soldr wheel --target <triple>` resolves the target (friendly aliases
    /// included), prepares the sysroot, provisions maturin, and tags the
    /// wheel `manylinux_2_17` / `musllinux_1_2` / `pypi` by target family.
    /// Arguments after `--target` are forwarded to `maturin build` verbatim.
    /// abi3 only in this first cut: a non-abi3 extension needs a CPython
    /// built for the target, which is refused rather than silently degraded.
    Wheel(crate::wheel_cmd::WheelArgs),
    /// Run cargo through soldr (cached, pinned toolchain)
    Cargo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Prepare Dylint dependencies or run the Dylint extension
    Dylint {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run Soldr's cache-aware validation suites
    ///
    /// `soldr lint` runs formatting validation, Clippy, and configured
    /// Dylint libraries with one canonical workspace scope. `lint deps`
    /// runs dependency/policy checks concurrently without starting the
    /// compiler cache. `lint ci` statically validates CI/build surfaces
    /// (workflows, composite actions, helper scripts) — it needs no Cargo
    /// manifest and starts no compiler cache; use `--format json` for a
    /// machine-readable report. `lint all` extends every suite (including
    /// `ci`) with udeps and semver-checks.
    Lint {
        /// Suite selector (`rust`, `deps`, `ci`, or `all`). Rust/deps/all
        /// accept cargo scope flags; `ci` accepts only `--format json|human`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run an arbitrary command with rustup's toolchain bin dir prepended to PATH (#1059)
    ///
    /// Workaround for Windows hosts where a Chocolatey-installed
    /// standalone Rust shadows rustup's `cargo` proxy. Cargo extensions
    /// like `cargo-dylint` / `cargo-binstall` invoke `"cargo"` directly
    /// (not through soldr), find the Chocolatey shim on PATH, and fail
    /// to honor per-crate `rust-toolchain.toml` overrides.
    ///
    /// `soldr exec <cmd> [args...]` resolves rustup's cargo via
    /// `rustup which cargo`, prepends its containing directory to PATH
    /// for the child process, and execs `<cmd>` unchanged. Any
    /// subprocess `<cmd>` spawns will then see rustup's proxy first.
    ///
    /// Example: `soldr exec cargo-dylint dylint --all`
    Exec {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Compile Rust source via the pinned toolchain
    Rustc {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Format Rust source via the pinned toolchain
    Rustfmt {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run the clippy linter via the pinned toolchain
    #[command(name = "clippy-driver")]
    ClippyDriver {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Drop-in passthrough to the system rustup binary
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

    /// Generate Rust documentation
    Rustdoc {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run the rust-analyzer language server
    #[command(name = "rust-analyzer")]
    RustAnalyzer {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Debug with gdb + Rust pretty-printers
    #[command(name = "rust-gdb")]
    RustGdb {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Debug with lldb + Rust pretty-printers
    #[command(name = "rust-lldb")]
    RustLldb {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Install the channel declared in rust-toolchain.toml
    Toolchain {
        #[command(subcommand)]
        subcommand: ToolchainSubcommand,
    },
    /// Install rustup itself into soldr's managed bin dir
    #[command(
        long_about = "Install `rustup` itself into the soldr-managed bin dir when the host has no system-managed toolchain manager. Idempotent: a re-run with rustup already present prints the resolved path and exits 0.\n\nFetches `rustup-init` from `https://static.rust-lang.org/rustup/dist/<host-triple>/` under the same `SOLDR_TRUST_MODE` / `SOLDR_CHECKSUMS_FILE` policy as every other soldr-fetched binary. Set `SOLDR_NO_BOOTSTRAP=1` to disable the implicit auto-install that runs from `soldr cargo` / `soldr rustup ...` when rustup is missing."
    )]
    Bootstrap {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Report drift between rust-toolchain.toml and rustup
    #[command(
        long_about = "Diagnose drift between `rust-toolchain.toml` and the currently installed rustup state. Read-only: never mutates rustup. Exit code is `1` when drift is detected, `0` otherwise."
    )]
    Doctor {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Force a fresh Defender real-time-scan probe of the soldr
        /// cache directory, ignoring the cached result. No-op outside
        /// Windows. Issue #357.
        #[arg(long)]
        refresh_defender_probe: bool,
        /// Delete an unmanaged extensionless `soldr` that is shadowing the
        /// installed `soldr.exe` on an MSYS PATH (issue #1979).
        ///
        /// Opt-in on purpose: a plain `soldr doctor` only reports the
        /// shadowing. Removing a binary from PATH is not something a
        /// diagnostic should do as a side effect, but no installer owns
        /// this file, so without an explicit switch an affected machine
        /// cannot be repaired by any soldr command.
        #[arg(long)]
        remove_shadowing_shim: bool,
    },
    /// Install per-version PATH shims for Rust tools
    ///
    /// Writes `cargo`, `rustc`, `rustfmt`, `clippy-driver`, and
    /// `rustdoc` shims under `~/.soldr/v<MANAGED_SHIM_VERSION>/shims/`
    /// and emits stable JSON describing where they live.
    Shims {
        /// Emit the stable machine-facing JSON form
        /// (`schema_version: 1`).
        #[arg(long)]
        json: bool,
    },

    /// Prebuild dependencies via bundled cargo-chef
    #[command(
        long_about = "Content-addressable dependency prebuild via the bundled `cargo-chef`. Splits a project build into a recipe phase (`cargo chef prepare`) and a stub-project compile phase (`cargo chef cook`) so the dep set can be cached as an output layer (Docker), a tarball (CI), or just a warm `target/` (local dev) that survives source-code commits.\n\nRoutes both phases through the cargo front door so zccache, `ZCCACHE_PATH_REMAP=auto`, and the soldr-managed toolchain homes all apply.\n\nRecognised flags (everything else: pass after `--`): `--release`, `--target <triple>`, `--workspace`, `--profile <name>`, `-p`/`--package <name>` (repeatable), `--recipe-path <path>`, `--keep-recipe`, `--prepare-only`, `--cook-only`."
    )]
    Cook {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show cache status and active toolchain
    Status {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
    },
    /// Inspect compilation cache entries
    Cache {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<CacheSubcommand>,
    },
    /// Inspect soldr's runtime logs (issue #820)
    ///
    /// `list` shows recent build launches, `show` expands one launch
    /// into cache stats / slow compiles / log paths, and `paths`
    /// prints every directory soldr writes runtime logs into.
    Logs {
        #[command(subcommand)]
        command: Option<LogsSubcommand>,
    },
    /// Show or set soldr configuration
    Config,
    /// Clear the embedded zccache build cache
    Clean,
    /// Purge all soldr-managed cache artifacts
    Purge,
    /// Review reclaimable cargo target/ directories
    ///
    /// Aliases: `purge-targets` (matches issue #234's `soldr --purge`
    /// wording). Uses the soldr registry (`~/.soldr/state.redb`) to
    /// discover tracked Cargo `target/` directories.
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
    /// Bundle a build cache + source mtimes into a .tar.zst
    #[command(
        long_about = "Bundle a build-cache directory plus a content-verified snapshot of source-file mtimes into a single `.tar.zst` archive. The output is consumed by `soldr load` to restore both the cache and Cargo-friendly source mtimes on a fresh checkout."
    )]
    Save(save_load::SaveArgs),
    /// Restore a soldr-save archive on a fresh checkout
    #[command(
        long_about = "Restore an archive produced by `soldr save`: unpack the cache to the destination directory and replay each source-file mtime, but only when the current file's size and BLAKE3 hash still match the snapshot so soldr cannot underbuild after a real source change."
    )]
    Load(save_load::LoadArgs),

    /// Bundle or extract a soldr release .tar.zst archive
    #[command(
        long_about = "Bundle a previously-built `soldr` release binary together with `soldr-daemon` and bundled tools into a single `.tar.zst` archive, or extract one for validation.\n\nBy default, `soldr archive` resolves the built soldr binary and daemon from `target/<triple>/release/` and optional managed tools from the soldr cache. In release CI, pass `--stage-dir <DIR>` to compress an already-validated staging directory with soldr's in-process zstd encoder instead of shelling out to `tar | zstd`. For smoke tests, pass `--input <FILE> --extract-dir <DIR>` to extract through soldr's in-process zstd decoder.\n\nThe output is a flat `.tar.zst` (zstd level 19) — every entry sits at the archive root unless `--stage-dir` contains a nested debug-info sidecar directory such as a dSYM bundle."
    )]
    Archive {
        /// Target triple to bundle for. Defaults to the auto-detected
        /// host triple.
        #[arg(long, value_name = "TRIPLE")]
        target: Option<String>,
        /// Existing staging directory to archive as-is. Used by release
        /// CI after it has staged and validated every required binary.
        #[arg(long, value_name = "DIR")]
        stage_dir: Option<std::path::PathBuf>,
        /// Existing soldr release archive to extract.
        #[arg(long, value_name = "FILE")]
        input: Option<std::path::PathBuf>,
        /// Destination directory for `--input` extraction.
        #[arg(long, value_name = "DIR")]
        extract_dir: Option<std::path::PathBuf>,
        /// Destination archive path. Suggest the `.tar.zst` suffix.
        #[arg(long, value_name = "FILE")]
        output: Option<std::path::PathBuf>,
    },

    /// Prepare the cross-compile toolchain for a target triple
    #[cfg_attr(
        any(),
        command(
            name = "prepare",
            long_about = "Uniform cross-compile toolchain bootstrap. Same invocation shape for every target — only `--target` varies. Internally dispatches based on the triple:\n\n  *-pc-windows-msvc:  ensure LLVM toolchain + extract the vendored xwin MSVC CRT cache from soldr-toolchain assets, then export target-scoped Cargo/cc-rs/linker env for the blessed `soldr build` path and explicit cargo-xwin fallback consumers.\n  x86_64-pc-windows-gnu: on Windows x64, ensure managed MinGW-w64 GCC plus GNU syslibs and export target-scoped Cargo/cc-rs env; on other hosts, fail visibly instead of falling back to cargo-zigbuild.\n  *-apple-darwin:     ensure the target-shaped Apple SDK and print `SDKROOT=<path>` (and append to $GITHUB_ENV when --github-env is set). `soldr build --target` is the blessed Darwin cross-build path; prepare is for env export and legacy/external tooling.\n  *-unknown-linux-*:  ensure cargo-zigbuild + zig (when triple != host).\n  All targets:        `rustup target add <triple>`.\n\nCollapses the per-step ad-hoc downloads in `cross-compile-all-targets.yml` into a single 'Preparing Cross Compile Toolchain' step. Designed to be wrapped by `.github/actions/prepare-cross-toolchain/action.yml`."
        )
    )]
    #[command(
        long_about = "Prepare soldr's complete target lifecycle. The invocation shape is identical for every canonical target: select an alias or Rust triple with `--target`; soldr installs the Rust standard library, selects and materializes the blessed compiler/linker plus SDK or sysroot, and exports the target-scoped environment. The same preparation is consumed by build, clippy, test compilation, nextest archives, and PEP 517 operations. Legacy backend wrappers are diagnostic-only overrides and are never selected by this command."
    )]
    Prepare {
        /// Target alias or triple to prepare the toolchain for. Three shapes
        /// are accepted:
        ///
        ///   * a single triple, e.g. `x86_64-pc-windows-msvc`;
        ///   * a comma-separated list, e.g.
        ///     `x86_64-pc-windows-msvc,aarch64-apple-darwin` —
        ///     useful for docker-image bake steps where no workspace
        ///     `Cargo.toml` is mounted yet;
        ///   * the literal `all` — expands to every triple declared
        ///     under `[workspace.metadata.soldr].targets` (or
        ///     `[package.metadata.soldr].targets`) in the nearest
        ///     `Cargo.toml`.
        ///
        /// See zackees/soldr#914.
        #[arg(long, value_name = "TARGET[,TARGET...]|all")]
        target: String,
        /// Optional path to append `KEY=VALUE` env-var lines (e.g.
        /// `SDKROOT=<path>` for darwin lanes). When running under
        /// GitHub Actions, point at `$GITHUB_ENV`. Export is limited
        /// to one target because global PATH/flag keys cannot safely
        /// represent several target lifecycles in one job.
        #[arg(long, value_name = "FILE")]
        github_env: Option<std::path::PathBuf>,
        /// Capture the prepared state (zig + LLVM + Apple SDK + xwin
        /// cache) into a single `tar.zst` archive at this path after
        /// preparation completes. Designed for `actions/cache@v4`'s
        /// save step on GitHub Actions so subsequent runs can
        /// `--restore` instead of re-downloading.
        #[arg(long, value_name = "FILE")]
        save: Option<std::path::PathBuf>,
        /// Extract a previously-saved archive BEFORE running the
        /// normal prepare flow. Anything still missing after restore
        /// is downloaded normally — partial restores are non-fatal.
        #[arg(long, value_name = "FILE")]
        restore: Option<std::path::PathBuf>,
    },

    /// soldr#938 — print the cross-compile env block (shell-eval).
    #[command(
        name = "env",
        long_about = "Print the cross-compile env block soldr would set internally for the given target, in shell-eval form. Use to bridge env into shells/IDE integrations that bypass `soldr cargo`:\n\n  eval \"$(soldr env --target mac-arm64)\"\n  soldr env --target win-x64 --shell-export   # `export KEY=VALUE` for sh/bash/zsh\n  soldr env --target linux-x64-musl --json    # stable JSON for tooling\n\nResolves the target via the same alias table soldr build uses (`win-x64`, `mac-arm64`, etc.; or Rust triple). JSON output includes the shared target-aware PyO3 plan. PYO3_NO_PYTHON is emitted only for a workspace-metadata-proven ABI3 cross extension; target Python compatibility assets are opt-in and separate from OS SDK preparation. See soldr#1610 + soldr#1614."
    )]
    Env {
        /// Target triple OR soldr alias (e.g. `win-x64`, `mac-arm64`,
        /// `linux-x64-musl`, `apple-silicon`, `x86_64-pc-windows-msvc`).
        /// See `crate::target_alias` for the full alias table.
        #[arg(long, value_name = "TRIPLE-OR-ALIAS")]
        target: String,
        /// Emit `export KEY=VALUE` lines (shell-export form) suitable
        /// for `eval`. Default is bare `KEY=VALUE` lines which `set
        /// -a` users can also source.
        #[arg(long)]
        shell_export: bool,
        /// Emit the same env block in stable JSON form. Mutually
        /// exclusive with --shell-export.
        #[arg(long, conflicts_with = "shell_export")]
        json: bool,
    },

    /// Cross-compile a soldr-bundled tool (crgx, cargo-chef) for a target triple
    #[command(
        name = "build-from-source",
        long_about = "Source-build a whitelisted soldr-managed tool (today: `crgx`, `cargo-chef`) for an arbitrary Rust target triple and deposit the resulting binary into `~/.soldr/bin/<tool>-from-source/<version>/<triple>/<tool>[.exe]` with a sha256 sidecar.\n\nMotivation (sub-issue #859 of meta #853): the release pipeline already source-builds these tools in some lanes because upstream does not always ship prebuilt binaries for every target — notably `aarch64-apple-darwin` for cargo-chef. This verb lifts that ad-hoc shell into a first-class soldr command:\n\n  soldr build-from-source crgx --target aarch64-apple-darwin\n  soldr build-from-source cargo-chef --target aarch64-apple-darwin\n\nResolution:\n  * `--target` defaults to the auto-detected host triple.\n  * `--version` defaults to the registry pin in `known_tools::lookup_by_crate(<tool>).pinned_version`.\n  * The build invokes `cargo install <tool>@<version> --target <triple> --root <staging> --locked --force` via the directly-resolved cargo binary, clearing inherited `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER`. This is the same direct-cargo pattern `soldr toolchain prepare` uses for plugin installs.\n\nOutputs:\n  * The installed binary lands at `~/.soldr/bin/<tool>-from-source/<version>/<triple>/<tool>[.exe]`.\n  * A sibling `<tool>.sha256` sidecar records the binary's sha256 in `<hash>  <name>` format compatible with `sha256sum -c`."
    )]
    BuildFromSource {
        /// Whitelisted tool name. Supported today: `crgx`, `cargo-chef`.
        #[arg(value_name = "TOOL")]
        tool: String,
        /// Target triple to build for. Defaults to the auto-detected
        /// host triple.
        #[arg(long, value_name = "TRIPLE")]
        target: Option<String>,
        /// Crate version to install (without the leading `v`). Defaults
        /// to the registry pin (`known_tools::lookup_by_crate(<tool>).
        /// pinned_version`).
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
    },

    /// Manage the long-lived soldr-daemon process
    Daemon {
        #[command(subcommand)]
        command: DaemonSubcommand,
    },
    /// Start a zccache session and print its id
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
    /// End a zccache session and emit its stats
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
    /// Apply platform-specific hot-cache tuning
    #[command(
        long_about = "Apply platform-specific hot-cache optimizations (Windows Defender exclusions today; future platforms TBD). Auto-skips on CI. See `docs/API.md` for the full matrix."
    )]
    Optimize(optimize::OptimizeArgs),
    /// Manage Windows Defender exclusions for soldr caches
    ///
    /// Self-documenting verbs (`check` / `add` / `remove`) over the same
    /// Defender machinery `soldr optimize` already exposes. Windows-only;
    /// no-op with a clear message on macOS / Linux.
    #[command(name = "defender-exclusions")]
    DefenderExclusions {
        #[command(subcommand)]
        subcommand: DefenderExclusionsSubcommand,
    },
    /// Show version
    #[command(hide = true)]
    Version {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
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
        #[arg(long, value_name = "SECS", default_value_t = 0)]
        idle_timeout: u64,
    },
    /// Ask the running daemon to shut down gracefully.
    Stop,
    /// Print the daemon's status (uptime, pid, request count).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Write soldr-daemon's running-process service definition.
    #[command(name = "install-servicedef")]
    InstallServiceDef {
        /// Override the soldr-daemon binary path. Defaults to a sibling
        /// of the current soldr executable.
        #[arg(long, value_name = "PATH")]
        daemon_binary: Option<std::path::PathBuf>,
        /// Emit the installed path and deferred broker-adoption items as JSON.
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
pub(crate) enum DefenderExclusionsSubcommand {
    /// Report the soldr-owned paths that should be excluded, and which
    /// of them soldr believes it has already added (from the local
    /// managed-exclusions tracking file). Does not require admin.
    Check {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Add soldr's hot cache directories to Defender's exclusion list.
    /// Requires admin elevation; UAC self-relaunches on Windows when
    /// the parent shell is non-admin.
    Add {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Print what would change without invoking PowerShell.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove soldr-managed exclusions from Defender. Never touches
    /// user-added entries. Requires admin elevation.
    Remove {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Print what would change without invoking PowerShell.
        #[arg(long)]
        dry_run: bool,
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
    /// One-shot "make sure this host can build" verb (issue #407
    /// Phase 2): auto-bootstraps `rustup` if missing, runs the same
    /// install + component + target + plugin steps as `prepare`, then
    /// smoke-verifies the resolved toolchain by spawning
    /// `cargo --version` and `rustc --version`. Used by `setup-soldr`
    /// (`setup-soldr#133`) to delegate its TS toolchain logic to the
    /// soldr binary.
    Ensure {
        /// Emit the stable machine-facing JSON form
        /// (`schema_version: 1`) for consumption by setup-soldr and
        /// other tooling. See `docs/API.md` for the full schema.
        #[arg(long)]
        json: bool,
    },
    /// Write PATH shim files into `--shim-dir` so a child process that
    /// resolves `cargo` / `rustfmt` / `clippy-driver` / `rustc` /
    /// `rustdoc` from PATH gets routed back through `soldr <tool>`
    /// (issue #407 Phase 3, ports setup-soldr's `ensure-shims.ts`).
    ///
    /// Idempotent by default: a shim file whose contents already match
    /// the expected body is left alone. `--force` overwrites regardless.
    /// `--json` emits the same `schema_version: 1` style payload
    /// `setup-soldr#133` consumes from `ensure`.
    Link {
        /// Destination directory for the shim files. Created if missing.
        #[arg(long, value_name = "PATH")]
        shim_dir: std::path::PathBuf,
        /// Emit the stable machine-facing JSON form
        /// (`schema_version: 1`) for consumption by setup-soldr and
        /// other tooling.
        #[arg(long)]
        json: bool,
        /// Overwrite existing shim files regardless of their current
        /// contents. Without `--force`, files whose contents differ are
        /// left untouched.
        #[arg(long)]
        force: bool,
    },
    /// Run env-detection probes (musl-cc availability, pre-populated
    /// `target/` warning, host triple summary) and emit either a
    /// human-readable summary or the stable `schema_version: 1` JSON
    /// payload consumed by `setup-soldr#133` (issue #407 Phase 4,
    /// ports the env-detection halves of setup-soldr's
    /// `detect-musl-cc.ts`, `detect-shared-target-warning.ts`, and
    /// `diagnostics.ts`).
    ///
    /// Namespaced under `toolchain` to avoid colliding with the
    /// top-level `soldr doctor` system check.
    Doctor {
        /// Emit the stable machine-facing JSON form
        /// (`schema_version: 1`) for consumption by setup-soldr and
        /// other tooling.
        #[arg(long)]
        json: bool,
    },
    /// soldr#988 Phase 2 — print the resolved soldr-toolchain
    /// catalogue origin and a one-line response summary (HTTP
    /// status, content-length, etag, last-modified). Honors
    /// `SOLDR_TOOLCHAIN_ORIGIN` (default
    /// `https://zackees.github.io/soldr-toolchain`).
    Catalogue {
        /// Emit the stable machine-facing JSON form
        /// (`schema_version: 1`) for tooling. Same shape conventions
        /// as the other `toolchain` JSON outputs.
        #[arg(long)]
        json: bool,
    },
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
        /// with shorthand flags. Report-only primary kinds are accepted
        /// for parse consistency but rejected before deletion.
        #[arg(
            long,
            value_enum,
            conflicts_with_all = [
                "registry_src",
                "git_checkouts",
                "target_incremental",
                "build_scripts",
                "doc",
                "subcommand_caches",
            ]
        )]
        kind: Option<GcListKind>,
        /// Shorthand for `--kind cargo_registry_src`. Walks
        /// `$CARGO_HOME/registry/src/<reg>/<crate>-<vers>/` and deletes
        /// the listed directories (#323 slice 2).
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "git_checkouts",
                "target_incremental",
                "build_scripts",
                "doc",
                "subcommand_caches",
            ]
        )]
        registry_src: bool,
        /// Shorthand for `--kind cargo_git_checkouts`. Walks
        /// `$CARGO_HOME/git/checkouts/<repo>/<commit>/` and deletes
        /// the listed directories (#323 slice 3).
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "registry_src",
                "target_incremental",
                "build_scripts",
                "doc",
                "subcommand_caches",
            ]
        )]
        git_checkouts: bool,
        /// Shorthand for `--kind cargo_target_incremental`.
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "registry_src",
                "git_checkouts",
                "build_scripts",
                "doc",
                "subcommand_caches",
            ]
        )]
        target_incremental: bool,
        /// Shorthand for `--kind cargo_target_build_script_binaries`.
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "registry_src",
                "git_checkouts",
                "target_incremental",
                "doc",
                "subcommand_caches",
            ]
        )]
        build_scripts: bool,
        /// Shorthand for `--kind cargo_target_doc`.
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "registry_src",
                "git_checkouts",
                "target_incremental",
                "build_scripts",
                "subcommand_caches",
            ]
        )]
        doc: bool,
        /// Shorthand for `--kind cargo_target_subcommand_caches`.
        #[arg(
            long,
            conflicts_with_all = [
                "kind",
                "registry_src",
                "git_checkouts",
                "target_incremental",
                "build_scripts",
                "doc",
            ]
        )]
        subcommand_caches: bool,
    },
    /// List every `target/` directory currently tracked in the soldr
    /// registry, without applying any age or size thresholds.
    List {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        /// Narrow the listing to a single taxonomy kind.
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
    /// Walk a configurable root (default `~/dev`, override via `--root`
    /// or `SOLDR_GC_TARGET_ROOT`) for every workspace with a sibling
    /// `target/` directory, then either report (default) or purge them
    /// (issue #574). Designed for cross-repo `target/` reclamation —
    /// independent of the per-repo `target/` taxonomy walks above.
    Target(Box<GcTargetArgs>),
    /// Run one full maintenance pass against an explicitly supplied orphaned
    /// soldr root. Refuses live daemons, relative paths, and directory links;
    /// never discovers sibling product roots.
    Maintain {
        #[arg(long, value_name = "ABSOLUTE_PATH")]
        root: std::path::PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Issue #1286 (F5): run the auto-GC sweep synchronously in this
    /// process. Internal — the cargo front door spawns this detached at
    /// build end so the sweep survives the wrapper process exiting; the
    /// spawner owns the throttle, so this verb runs unconditionally.
    #[command(name = "auto-sweep", hide = true)]
    AutoSweep,
    /// Internal PEP517 delegate helper. Holds the root build lease until its
    /// stdin pipe closes; an abruptly terminated Python parent therefore
    /// releases the OS lock automatically.
    #[command(name = "hold-build-lease", hide = true)]
    HoldBuildLease,
}

#[derive(clap::Args)]
pub(crate) struct GcTargetArgs {
    /// Filesystem root to walk. Defaults to the value of
    /// `$SOLDR_GC_TARGET_ROOT`, falling back to `~/dev`.
    #[arg(long, value_name = "PATH")]
    pub(crate) root: Option<std::path::PathBuf>,
    /// Maximum walk depth. Default raised from 4 to 8 in #680 — clud's
    /// `/clud-pr` worktree layout puts targets at
    /// `~/dev/<repo>/.claude/worktrees/<branch>/target` (depth 6), and
    /// workspace-member projects routinely sit one or two levels deeper
    /// than that. `jwalk` scan time is roughly linear in depth, and a
    /// shallow `~/dev` adds <1s at depth 8.
    #[arg(long, default_value_t = 8, value_name = "N")]
    pub(crate) max_depth: usize,
    /// Report-only (the default).
    #[arg(long, conflicts_with = "purge")]
    pub(crate) dry_run: bool,
    /// Delete every reported `target/` directory after confirming a
    /// single y/n prompt (skipped when `--yes` is also passed).
    #[arg(long, conflicts_with = "dry_run")]
    pub(crate) purge: bool,
    /// Skip the interactive y/n prompt before purging. Without
    /// `--purge` this is a no-op.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Emit the stable machine-facing JSON form for this command.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Taxonomy kinds accepted by `gc list --kind` / `gc purge --kind`
/// (#323 slice 2). Unknown values are rejected at clap-parse time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum GcListKind {
    /// Workspace `target/` dirs tracked by the soldr registry.
    #[value(name = "cargo_target")]
    CargoTarget,
    /// `target/<profile>/incremental/` directories under tracked targets.
    #[value(name = "cargo_target_incremental")]
    CargoTargetIncremental,
    /// `target/<profile>/build/*/build-script-build*` binaries.
    #[value(name = "cargo_target_build_script_binaries")]
    CargoTargetBuildScriptBinaries,
    /// `target/doc/` directories under tracked targets.
    #[value(name = "cargo_target_doc")]
    CargoTargetDoc,
    /// Tool-owned cache directories under tracked targets.
    #[value(name = "cargo_target_subcommand_caches")]
    CargoTargetSubcommandCaches,
    /// `$CARGO_HOME/registry/src/<reg>/<crate>-<vers>/` extracted
    /// crate sources.
    #[value(name = "cargo_registry_src")]
    CargoRegistrySrc,
    /// `$CARGO_HOME/registry/cache/<reg>/*.crate` package archives.
    #[value(name = "cargo_registry_cache")]
    CargoRegistryCache,
    /// `$CARGO_HOME/git/checkouts/<repo>/<commit>/` git-source crate
    /// checkouts (#323 slice 3).
    #[value(name = "cargo_git_checkouts")]
    CargoGitCheckouts,
    /// `$CARGO_HOME/git/db/<repo>/` primary bare git clones.
    #[value(name = "cargo_git_db")]
    CargoGitDb,
    /// `$CARGO_HOME/bin/<bin>` binaries installed by cargo.
    #[value(name = "cargo_installed_binaries")]
    CargoInstalledBinaries,
    /// `$RUSTUP_HOME/toolchains/<channel>` installed Rust toolchains.
    #[value(name = "rustup_toolchain")]
    RustupToolchain,
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
    /// the latest per-session stats/history written by Soldr and, when an
    /// analyzer surface is available, produces per-tool/per-extension
    /// breakdowns over the per-session journal.
    Report {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Gracefully end the active session and stop soldr-daemon.
    ///
    /// Synchronous by default: checkpoints the embedded zccache service,
    /// requests Soldr-daemon shutdown, and does not return until the
    /// exact daemon generation that acknowledged the request exits. The
    /// caller can safely snapshot the cache directory after this succeeds.
    Shutdown {
        /// If set, copy the session log/journal/stats files into
        /// `<dir>/<session-id>/` after Soldr-daemon is quiescent. The
        /// directory (and any missing parents) is created on demand.
        #[arg(long, value_name = "DIR")]
        archive_logs: Option<std::path::PathBuf>,
        /// Skip the explicit pre-shutdown embedded-cache checkpoint
        /// (legacy flag name; debugging only). Graceful daemon shutdown
        /// still waits for its own cache flush to complete.
        #[arg(long)]
        no_depgraph_save: bool,
        /// Maximum time to wait for the acknowledged daemon generation to
        /// finish its graceful cache flush. Timing out never force-kills it.
        #[arg(long, value_name = "SECONDS", default_value_t = 300)]
        shutdown_timeout_seconds: u64,
        /// Skip the post-request poll that confirms the acknowledged daemon
        /// generation has actually exited. By default `shutdown` blocks
        /// until that generation exits (or the
        /// `--shutdown-timeout-seconds` deadline elapses); pass
        /// `--no-wait` only when you genuinely do not care
        /// (interactive shells). See soldr#383.
        #[arg(long)]
        no_wait: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Synchronously checkpoint the embedded zccache state without
    /// stopping Soldr-daemon.
    ///
    /// Returns 0 only when pending writes and the index writer drain and
    /// every persistence step reports completion. Pair with
    /// `cache shutdown` before archiving a live cache. See soldr#383.
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
    /// Remove a worktree-like directory robustly across Windows file-
    /// lock races with long-lived caching daemons (zccache,
    /// rust-analyzer). See soldr#710 for design.
    ///
    /// Tier 1: try inline `remove_dir_all`. POSIX always wins here
    /// (delete-on-close semantics); Windows wins in the no-handle case.
    /// Tier 2 (Windows fallback): on EACCES/EBUSY, atomically rename
    /// to a per-volume trash dir (`~/.soldr/trash-<volume>/<id>/`) and
    /// return immediately. Run `soldr cache sweep-trash` later (or
    /// from a periodic hook) to reclaim the bytes once the daemon
    /// idles.
    ///
    /// Intended consumers: the `clud-pr` skill's worktree teardown
    /// step; CI scripts that tear down per-PR build dirs.
    #[command(name = "release-worktree")]
    ReleaseWorktree {
        /// Path to remove (typically a `.claude/worktrees/<branch>/`
        /// directory).
        path: std::path::PathBuf,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Recursively delete every entry under `~/.soldr/trash-*/` that
    /// can currently be deleted. Per-entry failures are tolerated
    /// (daemon may still hold handles); re-run after the daemon idles
    /// to reclaim the rest. Pair with `release-worktree`.
    #[command(name = "sweep-trash")]
    SweepTrash {
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

/// `soldr logs` subcommand surface — issue #820.
///
/// `list` / `show` / `paths` are implemented; `view` / `prune` stay
/// follow-up work tracked by the issue.
#[derive(clap::Subcommand)]
pub(crate) enum LogsSubcommand {
    /// List recent cargo launches recorded by the soldr daemon DB.
    List {
        /// Maximum number of launches to return. Defaults to the most
        /// recent 10.
        #[arg(long, default_value_t = 10)]
        limit: u32,
        /// Emit the stable machine-facing JSON form for this command
        /// (`schema_version: 1`). Stable enough for consumers; field
        /// additions are additive.
        #[arg(long)]
        json: bool,
    },
    /// Show one launch summary, cache hit/miss counts, slow compiles,
    /// miss reasons, and log paths.
    Show {
        /// Launch id from `soldr logs list`. Exact decimal ids are
        /// accepted, as are unique decimal or lower-hex prefixes.
        launch_id: String,
        /// Emit the stable machine-facing JSON form for this command
        /// (`schema_version: 1`). Stable enough for consumers; field
        /// additions are additive.
        #[arg(long)]
        json: bool,
    },
    /// Print every directory soldr writes logs into, annotated with
    /// what each directory contains. Self-documenting escape hatch
    /// so an agent or human triaging a slow build can locate the
    /// right journal without grepping source. JSON form for tooling.
    Paths {
        /// Emit the stable machine-facing JSON form for this command
        /// (`schema_version: 1`). Stable enough for consumers; field
        /// additions are additive.
        #[arg(long)]
        json: bool,
    },
}

#[cfg(test)]
#[path = "cli_args_tests.rs"]
mod global_flag_tests;
