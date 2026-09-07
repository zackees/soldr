//! Recognising amalgamated C translation units (soldr#2781).
//!
//! A few C dependencies ship as *amalgamations* — the whole library
//! concatenated into one translation unit. `libsqlite3-sys` is the common
//! one, at 255,636 lines / ~8 MB in a single `cc` process at `-O3`. Every
//! other unit in a typical dependency graph is a few thousand lines, so this
//! is not "a longer compile"; it is a categorically different resource event,
//! and it is the one the OOM killer reaches for first under concurrent load.
//!
//! Published Rust crates can have the same shape even when their root source
//! is small: the registry form of zccache folds a multi-crate workspace into
//! one large rustc unit, while `kernal-api` centralizes the formerly separate
//! platform/profiling implementations. zccache owns the one canonical
//! capacity-semaphore -> fair shared/exclusive admission path, after cache-hit
//! classification and immediately before a compiler child is spawned. Soldr
//! contributes only its product-specific Rust-crate predicate through
//! zccache's embedded host-classifier hook.

use std::path::{Path, PathBuf};

use zccache::compiler::CompilerFamily;
use zccache::embedded::{HostAdmissionClassifier, HostAdmissionError, HostCompilerRequest};

/// Sources at least this large are treated as amalgamations.
///
/// The gap this sits in is enormous rather than delicate: `sqlite3.c` is
/// ~8 MB, and an ordinary hand-written `.c` is single-digit KB. Anything
/// between is rare, and a false positive costs one extra diagnostic line,
/// so the threshold is set low enough to catch smaller amalgamations
/// (`zstd`'s, for instance) without needing to enumerate them.
const AMALGAMATION_BYTES: u64 = 1_000_000;

/// Sources treated as amalgamations regardless of measured size.
///
/// soldr#2781 asks for the allowlist to *supplement* the threshold rather
/// than replace it — a table nobody has to maintain for the common case. It
/// earns its place for a vendored source that is split at build time, or one
/// whose size sits under the threshold on one version and over it on the
/// next.
const KNOWN_AMALGAMATIONS: &[&str] = &["sqlite3.c", "zstd.c", "rocksdb.cc"];

/// Rust units that need exclusive access while they compile without linking.
///
/// These are either published-workspace amalgamations or first-party analysis
/// units with the same memory shape.  `soldr_cli` is intentionally included:
/// CI run 33389568913 killed its nightly Dylint workspace-analysis compiler
/// child while Nextest owned the other shared slot.  The observed invocation
/// is `--crate-type lib` with metadata output, so the predicate preserves
/// ordinary linking forms while protecting the measured heavy analysis form.
///
/// zccache itself owns the built-in names `zccache`, `zccache_cli_core`, and
/// `zccache_daemon_core`. Repeating them here would put the two predicates back
/// on a drift path even though there is now only one lock.
const SOLDR_RUST_EXCLUSIVE_NON_LINKING_UNITS: &[&str] = &["kernal_api", "soldr_cli"];

/// Summed `--extern` rlib bytes at or above which a Rust `--test` link is
/// treated as heavy enough to need exclusive admission (soldr#3150).
///
/// This is the Rust analogue of [`AMALGAMATION_BYTES`], and it exists for the
/// same reason that constant does: a name table only catches the units someone
/// remembered to add. [`SOLDR_HEAVY_TEST_LINKS`] listed `soldr_cli` and
/// `soldr_daemon`, which are the *lib* unit-test links -- the tests below still
/// pin that, and both name `src/lib.rs`. But since soldr#2934 consolidated the
/// integration tests, the heaviest links in the workspace are eight *separate*
/// binaries (`broker`, `cache_gc`, `cargo_front_door`, `cook_dylint`, `daemon`,
/// `fetch_tools`, `guards`, `toolchain_env`), each compiled as
/// `--crate-name=<target> --test`, and none of those names matched. Per
/// soldr#2931 those are precisely the binaries that "each link the full soldr
/// graph into its own binary".
///
/// Measuring instead of naming fixes all eight at once and stays correct when a
/// ninth is added or one is renamed.
///
/// The threshold is calibrated, not guessed. Over a real compile journal, a
/// full-graph `--test` link sums 147-231 MB of `--extern` rlibs across 42-43
/// externs (max single rlib 83.9 MB), while trivial test binaries carry 0-4
/// externs and sum to a few MB. 64 MiB sits in an order-of-magnitude gap, so
/// nothing delicate depends on the exact value.
///
/// Deliberately narrow: only `--test` links are measured. That is the shape the
/// documented invariant is about ("these measured heavy links must not overlap
/// any other compiler child") and where the observed kills happened. Ordinary
/// `--crate-type lib` compiles list their externs without linking them and have
/// a much flatter profile, so they keep shared admission. Widening this to real
/// `bin` links is soldr#3152's job, with a measured estimate rather than a
/// second threshold.
const HEAVY_TEST_LINK_EXTERN_BYTES: u64 = 64 * 1024 * 1024;

/// Override for [`HEAVY_TEST_LINK_EXTERN_BYTES`], in bytes.
///
/// A machine much larger or much smaller than a hosted runner may want a
/// different boundary, and an operator debugging an OOM needs to be able to
/// lower it without a rebuild.
const HEAVY_TEST_LINK_BYTES_ENV: &str = "SOLDR_HEAVY_TEST_LINK_BYTES";

/// First-party test links measured to exceed the safe parallel-memory envelope.
///
/// Unlike the registry amalgamations above, these are not source amalgamations:
/// their test link pulls the complete daemon/cache service graph into one rustc
/// child. CI run 33384831827 killed `soldr_daemon`'s `--test` compiler child
/// while a Dylint library build held the other slot. The #3024 completion run
/// then reproduced `soldr_cli --test` dying twice at more than 5 GiB while a
/// different ordinary test target occupied the other slot each time. Neither
/// run incremented the job cgroup's OOM counters: the actionable invariant is
/// that these measured heavy links must not overlap any other compiler child.
/// Giving only these exact test-link forms exclusive admission preserves
/// parallelism for ordinary first-party crate compilation.
const SOLDR_HEAVY_TEST_LINKS: &[&str] = &["soldr_daemon", "soldr_cli"];

/// Extensions that name a C/C++ translation unit on a compiler command line.
const SOURCE_EXTENSIONS: &[&str] = &["c", "cc", "cpp", "cxx", "c++", "m", "mm"];

/// A translation unit judged large enough to deserve its own scheduling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Amalgamation {
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
}

impl Amalgamation {
    /// How this reads in a diagnostic: `sqlite3.c (8.4 MB)`.
    pub(crate) fn describe(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());
        format!("{name} ({:.1} MB)", self.bytes as f64 / 1_000_000.0)
    }
}

/// Soldr's additive classifier for the embedded service's canonical compiler
/// admission path.
///
/// zccache invokes this only after every cache-hit path has missed. The return
/// value is combined with zccache's built-in C/C++ and Rust predicates before
/// zccache acquires its own capacity semaphore and fair resource lock.
#[derive(Debug, Default)]
pub(crate) struct SoldrHostAdmissionClassifier;

impl HostAdmissionClassifier for SoldrHostAdmissionClassifier {
    fn requires_exclusive(
        &self,
        request: &HostCompilerRequest<'_>,
    ) -> Result<bool, HostAdmissionError> {
        let exclusive = request.family() == CompilerFamily::Rustc
            && soldr_rust_crate_requires_exclusive_access(request.args());
        if exclusive {
            // zccache owns the actual permit and emits its acquisition at
            // `tracing::info!`, while Soldr's detached daemon deliberately
            // records WARN-and-above. Keep this one-line request diagnostic:
            // it is rare, identifies the policy decision, and lets an
            // operator distinguish a classifier miss from an admission-gate
            // failure without turning on per-compile trace logging.
            eprintln!(
                "soldr-daemon: compiler admission requests exclusive access for Rustc crate {}",
                rust_crate_name(request.args()).unwrap_or("<unnamed>")
            );
        }
        Ok(exclusive)
    }
}

fn soldr_rust_crate_requires_exclusive_access(args: &[String]) -> bool {
    soldr_rust_crate_requires_exclusive_access_with(args, file_len)
}

/// [`soldr_rust_crate_requires_exclusive_access`] with the size lookup injected.
///
/// Split out so the byte-threshold rule is testable without staging hundreds of
/// megabytes of real rlibs on disk. Production passes [`file_len`].
fn soldr_rust_crate_requires_exclusive_access_with(
    args: &[String],
    size_of: impl Fn(&Path) -> Option<u64>,
) -> bool {
    let Some(name) = rust_crate_name(args) else {
        return false;
    };

    if SOLDR_RUST_EXCLUSIVE_NON_LINKING_UNITS.contains(&name)
        && rust_crate_types_are_non_linking(args)
    {
        return true;
    }

    if !args.iter().any(|arg| arg == "--test") {
        return false;
    }

    if SOLDR_HEAVY_TEST_LINKS.contains(&name) {
        return true;
    }

    extern_rlib_bytes(args, size_of) >= heavy_test_link_threshold()
}

fn heavy_test_link_threshold() -> u64 {
    std::env::var(HEAVY_TEST_LINK_BYTES_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or(HEAVY_TEST_LINK_EXTERN_BYTES)
}

fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

/// Total bytes of the rlibs named by `--extern name=/path/to/lib.rlib`.
///
/// Cargo always passes these as absolute paths -- verified across 3,927
/// `--extern` arguments on `--test` lines in a real compile journal, of which
/// zero were relative -- so no working directory is needed and this stays a
/// pure function of the command line plus `stat`. That matters because the
/// classifier's [`HostCompilerRequest`] exposes no cwd.
///
/// Bare `--extern proc_macro` (no `=path`) names a sysroot crate with nothing
/// to measure and is skipped, as is any path that cannot be stat'd: an
/// unmeasurable input must not be able to turn admission into an I/O error, the
/// same rule [`measure`] follows.
fn extern_rlib_bytes(args: &[String], size_of: impl Fn(&Path) -> Option<u64>) -> u64 {
    extern_values(args)
        .filter_map(|value| value.split_once('='))
        .filter_map(|(_, path)| size_of(Path::new(path)))
        .sum()
}

fn extern_values(args: &[String]) -> impl Iterator<Item = &str> {
    let mut out = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--extern" {
            if let Some(value) = iter.next() {
                out.push(value.as_str());
            }
        } else if let Some(value) = arg.strip_prefix("--extern=") {
            out.push(value);
        }
    }
    out.into_iter()
}

fn rust_crate_name(args: &[String]) -> Option<&str> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--crate-name" {
            return args.next().map(String::as_str);
        }
        if let Some(name) = arg.strip_prefix("--crate-name=") {
            return Some(name);
        }
    }
    None
}

fn rust_crate_types_are_non_linking(args: &[String]) -> bool {
    if args.iter().any(|arg| arg == "--test") {
        return false;
    }

    let mut saw_crate_type = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let value = if arg == "--crate-type" {
            let Some(value) = args.next() else {
                return false;
            };
            value.as_str()
        } else if let Some(value) = arg.strip_prefix("--crate-type=") {
            value
        } else {
            continue;
        };

        for crate_type in value.split(',') {
            saw_crate_type = true;
            if !matches!(crate_type, "lib" | "rlib") {
                return false;
            }
        }
    }
    saw_crate_type
}

/// The amalgamated source in `args`, if there is one.
///
/// Deliberately measures the file rather than trusting the name: the point is
/// to recognise the *shape* of the work, and a private amalgamation nobody
/// added to [`KNOWN_AMALGAMATIONS`] is exactly the case a name table misses.
/// A path that cannot be measured is not an amalgamation — this runs on a
/// failure path and must not turn a compile error into an I/O error.
pub(crate) fn detect(args: &[String], cwd: &Path) -> Option<Amalgamation> {
    // `args[0]` is the compiler, not an input: both callers pass
    // `CompileRequest::args`, whose first element is the compiler path and
    // whose remainder is the compiler's own argv. Skipping it keeps a
    // pathological compiler path from being reported as the translation
    // unit, which would name the wrong file and leave the real amalgamation
    // unannounced.
    args.iter()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .filter(|arg| has_source_extension(arg))
        .find_map(|arg| measure(&resolve(arg, cwd)))
}

fn resolve(arg: &str, cwd: &Path) -> PathBuf {
    let path = PathBuf::from(arg);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn measure(path: &Path) -> Option<Amalgamation> {
    let bytes = std::fs::metadata(path).ok()?.len();
    let known = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| KNOWN_AMALGAMATIONS.contains(&n));
    (bytes >= AMALGAMATION_BYTES || known).then(|| Amalgamation {
        path: path.to_path_buf(),
        bytes,
    })
}

/// The line soldr emits *before* handing an amalgamation to the compiler.
///
/// Forewarning is the point. The post-mortem in `compiler_exit` can only
/// speak once the process has already been killed, and if the machine is
/// tight enough the user watches a build sit still and then die with no idea
/// which file was in the compiler's hands. This says so on the way in.
///
/// `eprintln!` rather than `tracing::info!`, for the reason `compile_limit`
/// records: the daemon installs its subscriber at `Level::WARN`, so an info
/// record is dropped and reaches nobody -- which would reproduce exactly the
/// undiscoverability this exists to fix. The detached daemon redirects stderr
/// into its log file, and `daemon start --foreground` shows it live.
pub(crate) fn compile_notice(args: &[String], cwd: &Path) -> Option<String> {
    detect(args, cwd).map(|unit| {
        format!(
            "soldr-daemon: INFO: compiling {} -- an amalgamated translation \
             unit, an entire library in one file. One compiler process holds \
             all of it, so this needs far more memory than an ordinary unit; \
             a build killed here is usually killed for memory, and lowering \
             CARGO_BUILD_JOBS / SOLDR_JOBS is what gives it room (soldr#2781).",
            unit.describe()
        )
    })
}

fn has_source_extension(arg: &str) -> bool {
    Path::new(arg)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SOURCE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

#[cfg(test)]
#[path = "amalgamation_tests.rs"]
mod tests;
