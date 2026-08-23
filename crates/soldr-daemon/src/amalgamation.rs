//! Recognising amalgamated C translation units (soldr#2781).
//!
//! A few C dependencies ship as *amalgamations* — the whole library
//! concatenated into one translation unit. `libsqlite3-sys` is the common
//! one, at 255,636 lines / ~8 MB in a single `cc` process at `-O3`. Every
//! other unit in a typical dependency graph is a few thousand lines, so this
//! is not "a longer compile"; it is a categorically different resource event,
//! and it is the one the OOM killer reaches for first under concurrent load.
//!
//! This module answers only "is this compile one of those?". It does not
//! schedule anything. The barrier soldr#2781 proposes needs the *compile*
//! semaphore, which lives in the embedded zccache service rather than here —
//! `compile_limit` sizes it, but the waiting happens inside zccache. Landing
//! detection first means the failure can at least name its cause today, and
//! the scheduler has something to ask when it exists.

use std::path::{Path, PathBuf};

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

/// The amalgamated source in `args`, if there is one.
///
/// Deliberately measures the file rather than trusting the name: the point is
/// to recognise the *shape* of the work, and a private amalgamation nobody
/// added to [`KNOWN_AMALGAMATIONS`] is exactly the case a name table misses.
/// A path that cannot be measured is not an amalgamation — this runs on a
/// failure path and must not turn a compile error into an I/O error.
pub(crate) fn detect(args: &[String], cwd: &Path) -> Option<Amalgamation> {
    args.iter()
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
}
