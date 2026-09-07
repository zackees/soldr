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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn write(dir: &Path, name: &str, bytes: usize) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, vec![b'x'; bytes]).expect("write fixture");
        path
    }

    #[test]
    fn a_large_translation_unit_is_detected_by_size_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "private-amalgamation.c", 2_000_000);
        let args = vec!["-O3".into(), "-c".into(), "private-amalgamation.c".into()];

        let found = detect(&args, dir.path()).expect("size alone must be enough");
        assert_eq!(found.bytes, 2_000_000);
        assert!(found
            .describe()
            .starts_with("private-amalgamation.c (2.0 MB)"));
    }

    #[test]
    fn an_ordinary_source_is_not_an_amalgamation() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "util.c", 4_096);
        let args = vec!["-O2".into(), "-c".into(), "util.c".into()];

        assert_eq!(detect(&args, dir.path()), None);
    }

    // The allowlist supplements the threshold; it does not replace it. A
    // known name under the size bar still counts, which is what makes the
    // table useful for a source that grows across versions.
    #[test]
    fn a_known_name_counts_even_when_small() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "sqlite3.c", 1_024);
        let args = vec!["-c".into(), "sqlite3.c".into()];

        assert!(detect(&args, dir.path()).is_some());
    }

    #[test]
    fn absolute_source_paths_are_measured_where_they_are() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "big.c", 1_500_000);
        let elsewhere = tempfile::tempdir().expect("second tempdir");
        let args = vec!["-c".into(), path.display().to_string()];

        assert!(detect(&args, elsewhere.path()).is_some());
    }

    // Runs on a failure path: a missing or unreadable source must produce
    // "no amalgamation", never an error or a panic.
    #[test]
    fn an_unmeasurable_source_is_simply_not_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = vec!["-c".into(), "absent.c".into()];

        assert_eq!(detect(&args, dir.path()), None);
    }

    // `-o sqlite3.o` and friends must not be mistaken for the input, and a
    // flag that merely ends in a source-looking extension is still a flag.
    #[test]
    fn flags_are_not_translation_units() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "sqlite3.c", 4_000_000);
        let args = vec!["--include=x.c".into(), "-Wp,-MD,dep.c".into()];

        assert_eq!(detect(&args, dir.path()), None);
    }

    #[test]
    fn non_source_arguments_are_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "libbig.a", 5_000_000);
        let args = vec!["-c".into(), "libbig.a".into()];

        assert_eq!(detect(&args, dir.path()), None);
    }

    // ---- the pre-compile notice (soldr#2781) ----------------------------
    //
    // These matter more than the post-mortem's: this is the line a user sees
    // *while* a 255,000-line translation unit is in the compiler, and it is
    // the only warning they get before an OOM kill takes the build with no
    // indication of which file was responsible.

    #[test]
    fn the_notice_names_the_file_and_its_size() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "sqlite3.c", 8_400_000);
        let args = vec!["-O3".into(), "-c".into(), "sqlite3.c".into()];

        let notice = compile_notice(&args, dir.path()).expect("an amalgamation must announce");

        assert!(notice.contains("sqlite3.c"), "{notice}");
        assert!(notice.contains("8.4 MB"), "{notice}");
    }

    #[test]
    fn the_notice_gives_the_cause_and_the_remedy() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "sqlite3.c", 8_400_000);
        let args = vec!["-c".to_string(), "sqlite3.c".into()];

        let notice = compile_notice(&args, dir.path()).expect("notice");

        // Reading this mid-build, the two questions are "why is this slow /
        // why did it die" and "what do I do".
        assert!(notice.contains("memory"), "{notice}");
        assert!(notice.contains("CARGO_BUILD_JOBS"), "{notice}");
        assert!(notice.contains("SOLDR_JOBS"), "{notice}");
        assert!(notice.contains("INFO"), "{notice}");
    }

    #[test]
    fn an_ordinary_compile_says_nothing() {
        // A notice on every `cc` invocation would be noise, and noise is how
        // the one that matters gets skipped.
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "util.c", 3_000);
        let args = vec!["-c".to_string(), "util.c".into()];

        assert_eq!(compile_notice(&args, dir.path()), None);
    }

    #[test]
    fn a_rustc_invocation_says_nothing() {
        // The daemon compiles rustc units through the same path. rustc splits
        // work across codegen units inside one invocation, so a large .rs is
        // not the single-process spike a large .c is -- and a notice here
        // would fire on ordinary Rust builds.
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "lib.rs", 4_000_000);
        let args = vec!["--edition=2021".to_string(), "lib.rs".into()];

        assert_eq!(compile_notice(&args, dir.path()), None);
    }

    // The notice is worthless if it arrives after the compile it describes.
    // `compile()` must call it before handing work to zccache -- the whole
    // point is forewarning, and a post-mortem already exists in
    // `compiler_exit`. Checked against the source because the emission is an
    // `eprintln!` that no stable in-process API can capture.
    #[test]
    fn the_compile_path_announces_before_dispatching() {
        let src = include_str!("zccache_embedded.rs");
        let announce = src
            .find("compile_notice(")
            .expect("compile() must ask for the notice");
        let dispatch = src
            .find("self.inner.compile(")
            .expect("compile() must dispatch to zccache");
        assert!(
            announce < dispatch,
            "the notice must be emitted before the compile it describes, \
             not after it returns"
        );
    }

    // The shape the daemon actually receives: `CompileRequest::args` carries
    // the compiler at [0] and the compiler's own arguments after it, and the
    // cwd arrives as a String. Every other test here passes bare flags, so
    // this is the one that would catch the detector being fed the wrong slice
    // -- `rustc_args` (args[1..]) instead of `args`, say, or a compiler path
    // being mistaken for an input.
    #[test]
    fn a_request_shaped_argv_finds_the_input_not_the_compiler() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "sqlite3.c", 8_400_000);
        let args: Vec<String> = [
            "/usr/bin/cc",
            "-O3",
            "-DSQLITE_CORE",
            "-c",
            "sqlite3.c",
            "-o",
            "sqlite3.o",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

        let found = detect(&args, dir.path()).expect("the input must be found");
        assert_eq!(found.path.file_name().unwrap(), "sqlite3.c");
        assert!(compile_notice(&args, dir.path()).is_some());
    }

    // A compiler whose own path ends in a source extension must not be
    // mistaken for the translation unit. Contrived, but the detector scans
    // args[0] too and the failure would be silent: the notice would name the
    // compiler and the real amalgamation would go unannounced.
    #[test]
    fn a_compiler_path_is_not_the_translation_unit() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "cc.c", 9_000_000);
        write(dir.path(), "real.c", 2_000_000);
        let compiler = dir.path().join("cc.c").display().to_string();
        let args = vec![compiler, "-c".to_string(), "real.c".to_string()];

        let found = detect(&args, dir.path()).expect("the real input is found");
        assert_eq!(
            found.path.file_name().unwrap(),
            "real.c",
            "args[0] is the compiler and must never be reported as the unit"
        );
    }

    #[test]
    fn soldr_policy_marks_kernal_api_for_exclusive_access() {
        let args = vec![
            "--crate-name".to_string(),
            "kernal_api".to_string(),
            "--crate-type=lib".to_string(),
            "/registry/kernal-api/src/lib.rs".to_string(),
        ];

        assert!(soldr_rust_crate_requires_exclusive_access(&args));
    }

    #[test]
    fn upstream_zccache_names_are_not_reclassified_by_soldr() {
        for crate_name in ["zccache", "zccache_cli_core", "zccache_daemon_core"] {
            let args = vec![
                format!("--crate-name={crate_name}"),
                "--crate-type=lib".to_string(),
            ];

            assert!(
                !soldr_rust_crate_requires_exclusive_access(&args),
                "{crate_name} belongs to zccache's built-in predicate"
            );
        }
    }

    #[test]
    fn linking_and_test_forms_of_kernal_api_keep_shared_access() {
        for suffix in [
            vec!["--crate-type=bin".to_string()],
            vec!["--crate-type=lib".to_string(), "--test".to_string()],
            vec!["--crate-type=lib,cdylib".to_string()],
        ] {
            let mut args = vec!["--crate-name=kernal_api".to_string()];
            args.extend(suffix);
            assert!(!soldr_rust_crate_requires_exclusive_access(&args));
        }
    }

    #[test]
    fn soldr_daemon_test_link_has_exclusive_access() {
        let args = vec![
            "--crate-name=soldr_daemon".to_string(),
            "--test".to_string(),
            "crates/soldr-daemon/src/lib.rs".to_string(),
        ];

        assert!(soldr_rust_crate_requires_exclusive_access(&args));
    }

    #[test]
    fn soldr_cli_test_link_has_exclusive_access() {
        let args = vec![
            "--crate-name=soldr_cli".to_string(),
            "--test".to_string(),
            "crates/soldr-cli/src/lib.rs".to_string(),
        ];

        assert!(soldr_rust_crate_requires_exclusive_access(&args));
    }

    /// The eight consolidated integration-test binaries from soldr#2934. None
    /// is named `soldr_cli` or `soldr_daemon`, so none matched the old name
    /// list -- and per soldr#2931 each links the full soldr graph, making them
    /// the heaviest links in the workspace.
    const CONSOLIDATED_TEST_BINARIES: &[&str] = &[
        "broker",
        "cache_gc",
        "cargo_front_door",
        "cook_dylint",
        "daemon",
        "fetch_tools",
        "guards",
        "toolchain_env",
    ];

    /// A `--test` link of `crate`, linking `externs` rlibs of `each` bytes.
    fn test_link(crate_name: &str, externs: usize, each: u64) -> (Vec<String>, u64) {
        let mut args = vec![
            format!("--crate-name={crate_name}"),
            "--test".to_string(),
            format!("crates/soldr-cli/tests/{crate_name}/main.rs"),
        ];
        for i in 0..externs {
            args.push("--extern".to_string());
            args.push(format!("dep{i}=/target/debug/deps/libdep{i}.rlib"));
        }
        (args, each)
    }

    fn sized(each: u64) -> impl Fn(&Path) -> Option<u64> {
        move |_| Some(each)
    }

    #[test]
    fn consolidated_integration_test_links_get_exclusive_access_by_size() {
        // 43 externs x 5 MB = 215 MB, matching the 147-231 MB measured for a
        // real full-graph test link.
        for name in CONSOLIDATED_TEST_BINARIES {
            let (args, each) = test_link(name, 43, 5_000_000);
            assert!(
                soldr_rust_crate_requires_exclusive_access_with(&args, sized(each)),
                "{name} links the full graph and must not share a compiler slot",
            );
        }
    }

    #[test]
    fn the_old_name_list_would_have_missed_every_one_of_them() {
        // The regression soldr#3150 records: these are exactly the binaries the
        // name-based rule could not see.
        for name in CONSOLIDATED_TEST_BINARIES {
            assert!(
                !SOLDR_HEAVY_TEST_LINKS.contains(name),
                "{name} is not in the name list -- that is the bug being fixed",
            );
        }
    }

    #[test]
    fn small_test_links_keep_shared_access() {
        // Trivial test binaries carry 0-4 externs in a real journal. They must
        // keep packing in parallel: serializing them would cost more build time
        // than the OOMs exclusivity prevents.
        for externs in [0_usize, 1, 2, 4] {
            let (args, each) = test_link("tiny_probe", externs, 1_000_000);
            assert!(
                !soldr_rust_crate_requires_exclusive_access_with(&args, sized(each)),
                "a {externs}-extern test link must not reserve the machine",
            );
        }
    }

    #[test]
    fn a_heavy_non_test_compile_keeps_shared_access() {
        // Only `--test` links are measured. An ordinary rlib compile lists its
        // externs without linking them and has a much flatter memory profile.
        let mut args = vec![
            "--crate-name=some_lib".to_string(),
            "--crate-type=lib".to_string(),
            "crates/some-lib/src/lib.rs".to_string(),
        ];
        for i in 0..43 {
            args.push("--extern".to_string());
            args.push(format!("dep{i}=/target/debug/deps/libdep{i}.rlib"));
        }
        assert!(!soldr_rust_crate_requires_exclusive_access_with(
            &args,
            sized(5_000_000)
        ));
    }

    #[test]
    fn unmeasurable_externs_do_not_fail_admission() {
        // An input that cannot be stat'd must not turn admission into an error
        // or a false positive -- the rule `measure` already follows.
        let (args, _) = test_link("broker", 43, 5_000_000);
        assert!(!soldr_rust_crate_requires_exclusive_access_with(
            &args,
            |_| None
        ));
    }

    #[test]
    fn both_extern_spellings_are_measured() {
        // `--extern k=v` and `--extern=k=v` both occur in real command lines.
        let split = vec![
            "--extern".to_string(),
            "a=/x/liba.rlib".to_string(),
            "--extern=b=/x/libb.rlib".to_string(),
        ];
        assert_eq!(extern_rlib_bytes(&split, |_| Some(10)), 20);
    }

    #[test]
    fn bare_sysroot_externs_are_skipped() {
        // `--extern proc_macro` names a sysroot crate with no path to measure.
        let args = vec![
            "--extern".to_string(),
            "proc_macro".to_string(),
            "--extern".to_string(),
            "a=/x/liba.rlib".to_string(),
        ];
        assert_eq!(extern_rlib_bytes(&args, |_| Some(7)), 7);
    }

    #[test]
    fn non_test_soldr_daemon_build_keeps_shared_access() {
        let args = vec![
            "--crate-name=soldr_daemon".to_string(),
            "--crate-type=lib".to_string(),
            "crates/soldr-daemon/src/lib.rs".to_string(),
        ];

        assert!(!soldr_rust_crate_requires_exclusive_access(&args));
    }

    #[test]
    fn soldr_cli_dylint_workspace_analysis_has_exclusive_access() {
        let args = vec![
            "--crate-name=soldr_cli".to_string(),
            "--crate-type=lib".to_string(),
            "--emit=dep-info,metadata".to_string(),
            "crates/soldr-cli/src/lib.rs".to_string(),
        ];

        assert!(soldr_rust_crate_requires_exclusive_access(&args));
    }

    #[test]
    fn an_ordinary_rust_crate_keeps_shared_access() {
        let args = vec![
            "--crate-name=small_crate".to_string(),
            "--crate-type=lib".to_string(),
            "/registry/small-crate/src/lib.rs".to_string(),
        ];

        assert!(!soldr_rust_crate_requires_exclusive_access(&args));
    }

    #[test]
    fn compile_dispatch_uses_only_zccaches_post_hit_resource_gate() {
        let src = include_str!("zccache_embedded.rs");
        assert!(
            !src.contains("compile_resource_gate"),
            "Soldr must not acquire a general compiler resource gate before \
             zccache knows whether the request is a cache hit"
        );
        assert!(
            src.contains("start_with_options_and_host_admission_classifier"),
            "Soldr's product-specific predicate must feed zccache's canonical \
             post-hit compiler admission"
        );
    }

    struct CountingSoldrPolicy {
        calls: Arc<AtomicUsize>,
    }

    impl HostAdmissionClassifier for CountingSoldrPolicy {
        fn requires_exclusive(
            &self,
            request: &HostCompilerRequest<'_>,
        ) -> Result<bool, HostAdmissionError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            SoldrHostAdmissionClassifier.requires_exclusive(request)
        }
    }

    type RecordedAdmission = (CompilerFamily, Vec<String>, bool);
    type RecordedAdmissions = Arc<Mutex<Vec<RecordedAdmission>>>;

    struct RecordingSoldrPolicy {
        requests: RecordedAdmissions,
    }

    impl HostAdmissionClassifier for RecordingSoldrPolicy {
        fn requires_exclusive(
            &self,
            request: &HostCompilerRequest<'_>,
        ) -> Result<bool, HostAdmissionError> {
            let exclusive = SoldrHostAdmissionClassifier.requires_exclusive(request)?;
            self.requests.lock().expect("recording policy lock").push((
                request.family(),
                request.args().to_vec(),
                exclusive,
            ));
            Ok(exclusive)
        }
    }

    // This is the behavior the removed Soldr-side gate could not provide: a
    // hit never reaches the product classifier or takes compiler admission.
    // Keep the test here, against the exact zccache release Soldr embeds, so a
    // future pin cannot silently move the callback ahead of hit detection.
    #[tokio::test]
    async fn pinned_embedded_hook_runs_after_cache_hit_classification() {
        use zccache::audit::{AuditId, AuditMode};
        use zccache::embedded::{
            AuditConfig, AuditContext, CompileRequest, HostIdentity, RuntimeHooks, ServiceLimits,
            ShutdownMode, ZccacheConfig, ZccacheService, ZccacheStartOptions,
        };

        let Some(compiler) = crate::test_support::find_on_path("cc") else {
            return;
        };
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("host-policy.c");
        let output = temp.path().join("host-policy.o");
        std::fs::write(&source, "int soldr_host_policy(void) { return 1; }\n")
            .expect("source fixture");
        let calls = Arc::new(AtomicUsize::new(0));
        let service = ZccacheService::start_with_options_and_host_admission_classifier(
            ZccacheConfig {
                host: HostIdentity {
                    product: "soldr-host-policy-test".into(),
                    instance_id: temp.path().display().to_string(),
                    workspace_id: "soldr-host-policy-workspace".into(),
                },
                cache_root: temp.path().join("cache").into(),
                audit: AuditConfig {
                    mode: AuditMode::Off,
                    ..AuditConfig::default()
                },
                limits: ServiceLimits::default(),
                runtime: RuntimeHooks::default(),
                cancellation: None,
            },
            ZccacheStartOptions::default(),
            Arc::new(CountingSoldrPolicy {
                calls: Arc::clone(&calls),
            }),
        )
        .await
        .expect("embedded service starts");
        let request = CompileRequest {
            audit: AuditContext::new(
                AuditId::new("soldr-host-policy-run").expect("run id"),
                AuditId::new("soldr-host-policy-trace").expect("trace id"),
            ),
            compiler: compiler.into(),
            args: vec![
                "-c".into(),
                source.display().to_string(),
                "-o".into(),
                output.display().to_string(),
            ],
            cwd: temp.path().into(),
            env: Vec::new(),
            stdin: Vec::new(),
        };

        let miss = service.compile(request.clone()).await.expect("cache miss");
        assert!(!miss.cached, "first compile must execute the compiler");
        assert_eq!(calls.load(Ordering::Relaxed), 1, "miss invokes policy");

        std::fs::remove_file(&output).expect("remove cold output");
        let hit = service.compile(request).await.expect("cache hit");
        assert!(hit.cached, "second compile must replay cached output");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "cache hit must bypass host policy and compiler admission"
        );
        service
            .shutdown(ShutdownMode::Graceful)
            .await
            .expect("shutdown");
    }

    // This reaches the exact production route: zccache identifies a real
    // rustc request, normalizes its compiler arguments, and only then calls
    // Soldr's embedded admission policy.  A pure argv test above cannot catch
    // a future zccache change that drops `--test` or calls the host policy
    // with a non-Rust family on this pipeline.
    #[tokio::test]
    async fn pinned_embedded_hook_marks_real_soldr_daemon_test_rustc_exclusive() {
        use zccache::audit::{AuditId, AuditMode};
        use zccache::embedded::{
            AuditConfig, AuditContext, CompileRequest, HostIdentity, RuntimeHooks, ServiceLimits,
            ShutdownMode, ZccacheConfig, ZccacheService, ZccacheStartOptions,
        };

        let Some(compiler) = crate::test_support::find_on_path("rustc") else {
            return;
        };
        let current_dir = std::env::current_dir().expect("resolve current directory");
        let repo = current_dir
            .ancestors()
            .find(|candidate| candidate.join("rust-toolchain.toml").is_file())
            .expect("find repository rust-toolchain.toml");
        let pinned_toolchain = crate::core::read_rust_toolchain_manifest(repo)
            .expect("read repository rust-toolchain.toml")
            .channel
            .expect("repository rust-toolchain.toml declares a channel");
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("workspace");
        std::fs::create_dir_all(project.join("src")).expect("create source directory");
        std::fs::write(
            project.join("src/lib.rs"),
            "#[test]\nfn admission_fixture() { assert_eq!(2 + 2, 4); }\n",
        )
        .expect("write source");
        let requests: RecordedAdmissions = Arc::new(Mutex::new(Vec::new()));
        let service = ZccacheService::start_with_options_and_host_admission_classifier(
            ZccacheConfig {
                host: HostIdentity {
                    product: "soldr-rustc-host-policy-test".into(),
                    instance_id: temp.path().display().to_string(),
                    workspace_id: "soldr-rustc-host-policy-workspace".into(),
                },
                cache_root: temp.path().join("cache").into(),
                audit: AuditConfig {
                    mode: AuditMode::Off,
                    ..AuditConfig::default()
                },
                limits: ServiceLimits::default(),
                runtime: RuntimeHooks::default(),
                cancellation: None,
            },
            ZccacheStartOptions::default(),
            Arc::new(RecordingSoldrPolicy {
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .expect("embedded service starts");
        let request = CompileRequest {
            audit: AuditContext::new(
                AuditId::new("soldr-rustc-host-policy-run").expect("run id"),
                AuditId::new("soldr-rustc-host-policy-trace").expect("trace id"),
            ),
            compiler: compiler.into(),
            args: vec![
                "--edition=2021".into(),
                "--crate-name=soldr_daemon".into(),
                "--test".into(),
                "--emit=metadata".into(),
                "--out-dir".into(),
                "target/debug/deps".into(),
                "src/lib.rs".into(),
            ],
            cwd: project.clone().into(),
            env: std::env::vars()
                .filter(|(key, _)| key != "RUSTUP_TOOLCHAIN")
                .chain(std::iter::once((
                    "RUSTUP_TOOLCHAIN".into(),
                    pinned_toolchain,
                )))
                .collect(),
            stdin: Vec::new(),
        };

        let response = service.compile(request).await.expect("rustc compile");
        assert_eq!(
            response.exit_code,
            0,
            "real rustc failed: {}",
            String::from_utf8_lossy(&response.stderr)
        );
        let recorded = {
            let mut guard = requests.lock().expect("recorded rustc request");
            std::mem::take(&mut *guard)
        };
        assert_eq!(recorded.len(), 1, "one cold rustc miss reaches the policy");
        let (family, args, exclusive) = &recorded[0];
        assert_eq!(*family, CompilerFamily::Rustc);
        assert!(args.iter().any(|arg| arg == "--crate-name=soldr_daemon"));
        assert!(args.iter().any(|arg| arg == "--test"));
        assert!(
            *exclusive,
            "real soldr_daemon test rustc needs exclusive admission"
        );
        service
            .shutdown(ShutdownMode::Graceful)
            .await
            .expect("shutdown");
    }
}
