//! Diagnostics for compiler processes terminated outside their own control.

/// zccache currently represents `ExitStatus::code() == None` as `-1`.
///
/// Without a diagnostic, the wrapper's exit guard misclassifies that result as
/// an internal SESSION transport fault. Preserve the compiler's stderr and add
/// the actionable distinction at the soldr/zccache response boundary.
pub(crate) fn annotate_signal_termination(
    exit_code: i32,
    stderr: Vec<u8>,
    args: &[String],
    cwd: &std::path::Path,
) -> Vec<u8> {
    let mut stderr = stderr;
    if crate::platform::process::exit::is_signal_termination(exit_code) {
        if !stderr.is_empty() && !stderr.ends_with(b"\n") {
            stderr.push(b'\n');
        }
        match crate::amalgamation::detect(args, cwd) {
            Some(unit) => stderr.extend_from_slice(amalgamation_diagnostic(&unit).as_bytes()),
            None => stderr.extend_from_slice(signal_diagnostic(args).as_bytes()),
        }
    }
    stderr
}

/// The kill we could not attribute to an amalgamation -- but can still name.
///
/// soldr#2828: this used to be a fixed string that named nothing, so a reader
/// got "a compiler process" and had to find the unit in cargo's next line. A
/// `Nextest Cacheability` run was killed compiling `soldr-cli`'s lib test --
/// 1340 tests in one translation unit, no C anywhere -- and the message said
/// only that *a* compiler died.
///
/// It also pointed at soldr#2453, which is closed and is about SESSION
/// compile-execution faults. The open, on-topic issue is soldr#2781.
fn signal_diagnostic(args: &[String]) -> String {
    let subject = match compilation_subject(args) {
        Some(name) => format!(" while compiling {name}"),
        None => String::new(),
    };
    format!(
        "soldr: compiler process was terminated by a Unix signal{subject}; under \
         concurrent build load this can indicate an OOM/resource-limit kill. \
         Reduce CARGO_BUILD_JOBS and SOLDR_JOBS, or inspect the host's \
         memory-pressure counters (soldr#2781).\n"
    )
}

/// What the compiler was working on, for a message rather than for logic.
///
/// `--crate-name <name>` is how rustc identifies a unit and is what soldr's own
/// cache lines print, so it is the name a reader is already looking at. Falling
/// back to the first non-flag argument covers compilers that take a bare source
/// path. `args[0]` is skipped for the reason `amalgamation::detect` documents:
/// it is the compiler, not an input.
fn compilation_subject(args: &[String]) -> Option<String> {
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if arg == "--crate-name" {
            return rest.next().cloned();
        }
        if let Some(name) = arg.strip_prefix("--crate-name=") {
            return Some(name.to_string());
        }
    }
    args.iter()
        .skip(1)
        .find(|arg| !arg.starts_with('-'))
        .cloned()
}

/// The same kill, but we know which file was in the compiler's hands.
///
/// soldr#2781: the generic advice is whole-build (`reduce CARGO_BUILD_JOBS`)
/// for what is usually one file, and the error names a C compiler rather than
/// anything the user wrote — so the reader has to already know that one
/// translation unit caused it. Naming the unit and its size makes the
/// remedy's scope obvious, and makes clear that the penalty is bounded to
/// this compile rather than the whole graph.
fn amalgamation_diagnostic(unit: &crate::amalgamation::Amalgamation) -> String {
    format!(
        "soldr: compiler process was terminated by a Unix signal while compiling \
         {}, an amalgamated translation unit -- an entire library in one file, \
         orders of magnitude larger than an ordinary compile. Under concurrent \
         build load this usually means it was killed for memory. Lowering \
         CARGO_BUILD_JOBS and SOLDR_JOBS gives it room, at the cost of \
         concurrency for the whole build (soldr#2781).\n",
        unit.describe(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Host-neutral: the per-host classification of `-1` lives in the
    // platform crate's exit tests; here the assertion is conditional on the
    // platform's own answer so the diagnostic path is covered on Unix hosts
    // and the no-op path is covered on Windows hosts.
    #[test]
    fn signal_termination_gets_an_actionable_diagnostic() {
        let stderr = annotate_signal_termination(-1, Vec::new(), &[], std::path::Path::new("."));
        if crate::platform::process::exit::is_signal_termination(-1) {
            assert_eq!(stderr, signal_diagnostic(&[]).as_bytes());
        }
    }

    /// soldr#2828: the generic message named nothing, so a reader got "a
    /// compiler process" and had to find the unit in cargo's next line.
    #[test]
    fn the_generic_diagnostic_names_the_rust_unit() {
        let args = vec![
            "/usr/bin/rustc".to_string(),
            "--crate-name".to_string(),
            "soldr_cli".to_string(),
            "src/lib.rs".to_string(),
        ];
        let text = signal_diagnostic(&args);
        assert!(
            text.contains("while compiling soldr_cli"),
            "must name the unit; got {text}"
        );
        assert!(
            text.contains("soldr#2781"),
            "must point at the open issue, not the closed soldr#2453: {text}"
        );
    }

    #[test]
    fn the_crate_name_may_be_joined_with_an_equals() {
        let args = vec!["rustc".to_string(), "--crate-name=soldr_daemon".to_string()];
        assert!(signal_diagnostic(&args).contains("while compiling soldr_daemon"));
    }

    /// The compiler is argv[0], not the thing being compiled -- naming it would
    /// report the wrong subject, which is the mistake `amalgamation::detect`
    /// documents for the same reason.
    #[test]
    fn the_compiler_path_is_never_reported_as_the_subject() {
        let args = vec!["/usr/bin/rustc".to_string()];
        let text = signal_diagnostic(&args);
        assert!(!text.contains("rustc"), "named the compiler: {text}");
        assert!(!text.contains("while compiling"), "nothing to name: {text}");
    }

    /// A bare source path is the subject when there is no `--crate-name`.
    #[test]
    fn a_bare_source_argument_is_used_when_there_is_no_crate_name() {
        let args = vec!["cc".to_string(), "-O2".to_string(), "foo.c".to_string()];
        assert!(signal_diagnostic(&args).contains("while compiling foo.c"));
    }

    #[test]
    fn compiler_stderr_is_preserved_before_the_diagnostic() {
        let original = b"rustc said why".to_vec();
        let stderr =
            annotate_signal_termination(-1, original.clone(), &[], std::path::Path::new("."));
        if crate::platform::process::exit::is_signal_termination(-1) {
            assert!(stderr.starts_with(b"rustc said why\n"));
            assert!(stderr.ends_with(signal_diagnostic(&[]).as_bytes()));
        } else {
            assert_eq!(stderr, original);
        }
    }

    #[test]
    fn ordinary_exit_codes_are_byte_identical() {
        let original = b"ordinary compiler error\n".to_vec();
        assert_eq!(
            annotate_signal_termination(1, original.clone(), &[], std::path::Path::new(".")),
            original
        );
    }

    // soldr#2781: the generic advice cannot say which file was in the
    // compiler's hands, and for an amalgamation that is the whole question.
    #[test]
    fn a_killed_amalgamation_is_named() {
        if !crate::platform::process::exit::is_signal_termination(-1) {
            return; // the classification is Unix-only; covered there.
        }
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("sqlite3.c"), vec![b'x'; 8_000_000]).expect("fixture");
        let args = vec!["-O3".to_string(), "-c".into(), "sqlite3.c".into()];

        let stderr = annotate_signal_termination(-1, Vec::new(), &args, dir.path());
        let text = String::from_utf8_lossy(&stderr);

        assert!(text.contains("sqlite3.c (8.0 MB)"), "{text}");
        assert!(text.contains("amalgamated translation unit"), "{text}");
        // The generic line would send the reader to the whole-build knob with
        // no idea one file caused it.
        assert!(
            !stderr.ends_with(signal_diagnostic(&args).as_bytes()),
            "{text}"
        );
    }

    // The other half of the same claim: an ordinary compile must not acquire
    // an amalgamation story it has no evidence for.
    #[test]
    fn an_ordinary_source_still_gets_the_generic_diagnostic() {
        if !crate::platform::process::exit::is_signal_termination(-1) {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("util.c"), vec![b'x'; 2_048]).expect("fixture");
        let args = vec!["-c".to_string(), "util.c".into()];

        let stderr = annotate_signal_termination(-1, Vec::new(), &args, dir.path());

        assert_eq!(stderr, signal_diagnostic(&args).as_bytes());
    }
}
