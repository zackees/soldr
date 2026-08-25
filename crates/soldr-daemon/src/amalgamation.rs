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
//! platform/profiling implementations. The resource gate below lets ordinary
//! units compile concurrently while either kind of oversized unit gets
//! exclusive access.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

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

/// Published Rust crates known to collapse a much more granular source
/// workspace into one rustc compilation unit.
const KNOWN_RUST_AMALGAMATIONS: &[&str] = &["kernal_api", "zccache"];

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

/// Fair shared/exclusive admission around the embedded compile service.
///
/// Tokio's write-preferring lock prevents a stream of ordinary compiles from
/// starving an oversized unit once it reaches the queue.
#[derive(Clone, Default)]
pub(crate) struct CompileResourceGate {
    inner: Arc<RwLock<()>>,
}

pub(crate) enum CompileResourcePermit {
    Shared { _guard: OwnedRwLockReadGuard<()> },
    Exclusive { _guard: OwnedRwLockWriteGuard<()> },
}

impl CompileResourceGate {
    pub(crate) async fn acquire(&self, exclusive: bool) -> CompileResourcePermit {
        if exclusive {
            CompileResourcePermit::Exclusive {
                _guard: self.inner.clone().write_owned().await,
            }
        } else {
            CompileResourcePermit::Shared {
                _guard: self.inner.clone().read_owned().await,
            }
        }
    }
}

/// Whether this request must run without another compiler process beside it.
pub(crate) fn requires_exclusive_access(args: &[String], cwd: &Path) -> bool {
    detect(args, cwd).is_some()
        || rust_crate_name(args).is_some_and(|name| KNOWN_RUST_AMALGAMATIONS.contains(&name))
}

fn rust_crate_name(args: &[String]) -> Option<&str> {
    let mut args = args.iter().skip(1);
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
    fn published_workspace_amalgamations_require_exclusive_access() {
        for crate_name in ["kernal_api", "zccache"] {
            let args = vec![
                "/toolchain/bin/rustc".to_string(),
                "--crate-name".to_string(),
                crate_name.to_string(),
                format!("/registry/{crate_name}/src/lib.rs"),
            ];

            assert!(
                requires_exclusive_access(&args, Path::new(".")),
                "{crate_name} must receive the oversized-unit resource gate"
            );
        }
    }

    #[test]
    fn an_ordinary_rust_crate_keeps_shared_access() {
        let args = vec![
            "/toolchain/bin/rustc".to_string(),
            "--crate-name=small_crate".to_string(),
            "/registry/small-crate/src/lib.rs".to_string(),
        ];

        assert!(!requires_exclusive_access(&args, Path::new(".")));
    }

    #[tokio::test]
    async fn an_exclusive_unit_waits_for_all_shared_units() {
        let gate = CompileResourceGate::default();
        let shared = gate.acquire(false).await;
        let waiting_gate = gate.clone();
        let mut waiting = tokio::spawn(async move { waiting_gate.acquire(true).await });

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut waiting)
                .await
                .is_err(),
            "exclusive access must wait while an ordinary compile is active"
        );
        drop(shared);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("exclusive access is granted after readers leave")
            .expect("gate task completes");
    }

    #[test]
    fn compile_dispatch_acquires_the_resource_gate_before_the_backend() {
        let src = include_str!("zccache_embedded.rs");
        let acquire = src
            .find("compile_resource_gate.acquire(")
            .expect("compile() must acquire the resource gate");
        let dispatch = src
            .find("self.inner.compile(")
            .expect("compile() must dispatch to zccache");
        assert!(
            acquire < dispatch,
            "resource admission must precede dispatch"
        );
    }
}
