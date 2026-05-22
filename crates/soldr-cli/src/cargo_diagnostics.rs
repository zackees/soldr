//! Post-failure cargo output scanner that rewraps recognizable build
//! errors with actionable, platform-aware hints.
//!
//! Today the module recognizes ONE category — `MissingHostTool` — where
//! a `build.rs` panicked because it tried to spawn an executable that's
//! not on PATH. The canonical trigger is a C-sys crate like
//! `tikv-jemalloc-sys` on a minimal Rust container (`rust:slim`,
//! `rust:bookworm-slim`, `catthehacker/ubuntu:act-*`) that ships
//! `cargo` but no `build-essential` / `autoconf` / `make`. The crate's
//! build script panics with `failed to execute command: No such file
//! or directory (os error 2)`, cargo bubbles that up as
//! `error: failed to run custom build command for \`<crate>\``, and
//! the user is left guessing which apt package they need.
//!
//! soldr already has cargo's stderr captured (for the CI/non-TTY path
//! in `cargo_front_door::run_cargo_capturing_failure_diagnostic_tail`)
//! and we exit non-zero with the same code cargo did — so the
//! diagnosis prints AFTER cargo's own error, framing what the user
//! just saw rather than replacing it.
//!
//! See issue #422 (`feat(bootstrap): detect / install system C build
//! deps so Rust crates with C-FFI build out of the box`) for the
//! Tier 1 (detect + report) design this module implements. Tier 2
//! (`soldr bootstrap --include-c-deps`) is a separate followup —
//! see the issue body.

/// Structured diagnosis derived from a failing cargo invocation.
/// `None` from [`detect_build_script_failure`] means we recognized
/// nothing actionable — the caller should fall through and let cargo's
/// own error stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildScriptDiagnosis {
    /// e.g. `"tikv-jemalloc-sys"` — taken from cargo's
    /// `error: failed to run custom build command for \`...\`` line.
    pub crate_name: String,
    /// Best-effort tool name extracted from the `running: "..."` line
    /// the crate's build script printed just before it panicked.
    /// `None` when the build script didn't log the spawn (e.g. it
    /// called `Command::new` directly without a preceding `println!`).
    pub command_name: Option<String>,
    pub category: DiagnosisCategory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiagnosisCategory {
    /// The build script tried to spawn a host executable that wasn't
    /// on PATH. Detected by the `os error 2` (ENOENT on POSIX, "The
    /// system cannot find the file specified" on Windows) class of
    /// IO error escaping a `build.rs` panic.
    MissingHostTool,
}

/// Marker line cargo prints when a `build.rs` exits non-zero.
const CARGO_BUILD_SCRIPT_FAILURE_MARKER: &str = "error: failed to run custom build command for";

/// Substrings within a `panicked at` payload that identify the
/// "ENOENT during spawn" failure mode regardless of host OS.
///
/// - POSIX: `No such file or directory (os error 2)`
/// - Windows: `The system cannot find the file specified. (os error 2)`
///
/// The `(os error 2)` suffix is the stable signal; the prose half
/// varies. We match either substring.
const MISSING_TOOL_PANIC_NEEDLES: &[&str] = &[
    "(os error 2)",
    "No such file or directory",
    "The system cannot find the file specified",
];

/// Substring that build scripts print to stdout (which cargo
/// surfaces as `--- stdout`) just before they spawn the failing
/// executable. The cargo / cc / autoconf ecosystem convergently
/// follows the `running: "<exe>" "<arg>" ...` pattern; the
/// double-quoted first token is the binary name.
const RUNNING_PREFIX: &str = "running: ";

/// Scan cargo's captured combined stdout+stderr for a recognizable
/// build-script failure pattern. Cheap, allocation-light, line-oriented.
///
/// Algorithm:
/// 1. Walk lines forward looking for the cargo failure marker
///    ([`CARGO_BUILD_SCRIPT_FAILURE_MARKER`]) and pull the crate name
///    out of it.
/// 2. From there, scan subsequent lines for the most recent
///    `running: "..."` (the build script's announcement of the
///    soon-to-fail spawn).
/// 3. Continue forward looking for a `panicked at .../build.rs:` line
///    and the following panic message. If the panic message contains
///    any of [`MISSING_TOOL_PANIC_NEEDLES`], we classify as
///    `MissingHostTool` and return.
/// 4. If no panic in the failure block matches, return `None`.
///
/// The scanner is intentionally conservative — false positives would
/// crowd out cargo's own diagnostics. Better to under-diagnose than
/// to mislead.
pub(crate) fn detect_build_script_failure(captured: &str) -> Option<BuildScriptDiagnosis> {
    let mut crate_name: Option<String> = None;
    let mut most_recent_running: Option<String> = None;
    let mut saw_build_rs_panic = false;

    for line in captured.lines() {
        let trimmed = line.trim_start();

        if let Some(rest) = trimmed.strip_prefix(CARGO_BUILD_SCRIPT_FAILURE_MARKER) {
            // The remainder is ` \`<crate name> <version>\``. Trim
            // whitespace, then extract the chunk between backticks.
            crate_name = parse_crate_name_from_marker(rest);
            // Reset state: every `error: failed to run custom build`
            // line starts a fresh failure block. cargo may report
            // multiple in one run if `--keep-going` etc.
            most_recent_running = None;
            saw_build_rs_panic = false;
            continue;
        }

        if crate_name.is_none() {
            // We haven't entered a failure block yet; ignore everything.
            continue;
        }

        if let Some(running) = parse_running_line(trimmed) {
            most_recent_running = Some(running);
            continue;
        }

        if line_indicates_build_rs_panic(trimmed) {
            saw_build_rs_panic = true;
            continue;
        }

        if saw_build_rs_panic && line_indicates_missing_host_tool(trimmed) {
            return Some(BuildScriptDiagnosis {
                crate_name: crate_name.unwrap_or_default(),
                command_name: most_recent_running,
                category: DiagnosisCategory::MissingHostTool,
            });
        }
    }

    None
}

/// Cargo prints the failing-crate line as:
/// ``error: failed to run custom build command for `tikv-jemalloc-sys v0.6.1+5.3.0` ``
/// (trailing backtick). Pull the chunk between backticks and drop the
/// version suffix.
fn parse_crate_name_from_marker(rest: &str) -> Option<String> {
    let start = rest.find('`')?;
    let after = &rest[start + 1..];
    let end = after.find('`')?;
    let inside = &after[..end];
    // Split on whitespace and take the first token — cargo always
    // emits `<crate> <version>` (sometimes also `(/path)` for path
    // deps); we want just the crate name.
    let name = inside.split_whitespace().next()?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// Pull the executable name out of a `running: "<exe>" "<arg>" ...`
/// line. cargo / cc / autoconf use this shape uniformly. Returns
/// `None` for any other line shape.
fn parse_running_line(line: &str) -> Option<String> {
    let rest = line.strip_prefix(RUNNING_PREFIX)?;
    // First token is `"<exe>"`. Find the first double quote, then
    // the next one.
    let after_open = rest.strip_prefix('"')?;
    let close = after_open.find('"')?;
    let exe = &after_open[..close];
    if exe.is_empty() {
        return None;
    }
    Some(exe.to_string())
}

/// Recognize a panic that came from a `build.rs`. The shape cargo
/// shows the user is one of:
///
///   `thread 'main' panicked at /.../build.rs:407:19:`
///   `panicked at .../build.rs:NNN:NNN`  (older rustc)
fn line_indicates_build_rs_panic(line: &str) -> bool {
    if !line.contains("panicked at") {
        return false;
    }
    line.contains("build.rs")
}

fn line_indicates_missing_host_tool(line: &str) -> bool {
    MISSING_TOOL_PANIC_NEEDLES
        .iter()
        .any(|needle| line.contains(needle))
}

/// Render the diagnosis as a multi-line user-facing message suitable
/// for stderr. Caller is expected to print this AFTER cargo's own
/// error block — it frames what the user already saw rather than
/// replacing it.
pub(crate) fn render_diagnosis(diag: &BuildScriptDiagnosis) -> String {
    match diag.category {
        DiagnosisCategory::MissingHostTool => render_missing_host_tool(diag),
    }
}

fn render_missing_host_tool(diag: &BuildScriptDiagnosis) -> String {
    let mut out = String::new();
    out.push_str("\nsoldr: cargo build failed inside the build script of `");
    out.push_str(&diag.crate_name);
    out.push_str("` — it tried to spawn an executable that isn't on PATH.\n");

    if let Some(cmd) = &diag.command_name {
        out.push_str("soldr: failing command: `");
        out.push_str(cmd);
        out.push_str("`\n");
    }

    out.push_str(
        "soldr: this is the classic \"minimal Rust container, no C toolchain\" trap — \
the crate uses a `-sys` build script that needs a host C compiler + autotools.\n",
    );
    out.push_str("soldr: install hints by platform:\n");
    out.push_str(
        "  - debian/ubuntu: sudo apt-get install -y build-essential autoconf pkg-config\n",
    );
    out.push_str("  - alpine:        apk add build-base autoconf pkg-config musl-dev\n");
    out.push_str("  - rhel/fedora:   sudo dnf install -y gcc gcc-c++ make autoconf pkgconfig\n");
    out.push_str("  - macos:         xcode-select --install\n");
    out.push_str("  - windows:       install \"Desktop development with C++\" via Visual Studio Build Tools\n");
    out.push_str(
        "soldr: see issue #422 for the followup that adds `--include-c-deps` to soldr bootstrap.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic cargo stderr modeled after the real tikv-jemalloc-sys
    /// failure on `rust:1.94.1-slim-bookworm`. Inline so the test stays
    /// hermetic — no fixture file shenanigans.
    const TIKV_JEMALLOC_FAILURE: &str = "\
   Compiling tikv-jemalloc-sys v0.6.1+5.3.0
error: failed to run custom build command for `tikv-jemalloc-sys v0.6.1+5.3.0`

Caused by:
  process didn't exit successfully: `/tmp/cargo-target/release/build/tikv-jemalloc-sys-7c3/build-script-build` (exit status: 101)
  --- stdout
  cargo:rerun-if-changed=jemalloc
  running: \"autogen.sh\" \"--prefix=/tmp/jemalloc\" \"--with-version=5.3.0-0-g54eaed1\"

  --- stderr

  thread 'main' panicked at /cargo-home/registry/src/index.crates.io-1949cf8c6b5b557f/tikv-jemalloc-sys-0.6.1+5.3.0-something/build.rs:407:19:
  failed to execute command: No such file or directory (os error 2)
  note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
";

    #[test]
    fn detects_tikv_jemalloc_sys_missing_autotools() {
        let diag = detect_build_script_failure(TIKV_JEMALLOC_FAILURE)
            .expect("tikv-jemalloc-sys failure must match");
        assert_eq!(diag.crate_name, "tikv-jemalloc-sys");
        assert_eq!(diag.command_name.as_deref(), Some("autogen.sh"));
        assert_eq!(diag.category, DiagnosisCategory::MissingHostTool);
    }

    #[test]
    fn detects_windows_variant_of_os_error_2_message() {
        // Windows rust toolchain emits a different prose form for the
        // same ENOENT-class error. The `(os error 2)` suffix is the
        // stable signal that lets us match both.
        let input = "\
error: failed to run custom build command for `openssl-sys v0.9.100`

Caused by:
  process didn't exit successfully: `target/build-script-build` (exit status: 101)
  --- stderr
  thread 'main' panicked at C:\\Users\\runner\\.cargo\\registry\\openssl-sys-0.9.100\\build.rs:512:9:
  failed to execute command: The system cannot find the file specified. (os error 2)
";
        let diag = detect_build_script_failure(input).expect("windows-shaped panic must match");
        assert_eq!(diag.crate_name, "openssl-sys");
        assert_eq!(diag.category, DiagnosisCategory::MissingHostTool);
    }

    #[test]
    fn command_name_falls_back_to_none_when_running_line_absent() {
        // build.rs that calls Command::new directly without a
        // preceding println!("running: ...") still triggers the
        // ENOENT classification — just without a known command name.
        let input = "\
error: failed to run custom build command for `bzip2-sys v0.1.13`

Caused by:
  process didn't exit successfully: `build-script-build` (exit status: 101)
  --- stderr
  thread 'main' panicked at /home/x/.cargo/registry/bzip2-sys-0.1.13/build.rs:88:33:
  failed to execute command: No such file or directory (os error 2)
";
        let diag = detect_build_script_failure(input).expect("must match");
        assert_eq!(diag.crate_name, "bzip2-sys");
        assert!(
            diag.command_name.is_none(),
            "no running: line → no command name; got {:?}",
            diag.command_name,
        );
    }

    #[test]
    fn returns_none_on_compile_error_not_build_script_failure() {
        // Plain rustc error — no `build.rs`, no missing-tool signal.
        // Must NOT trigger the diagnostic.
        let input = "\
   Compiling soldr-cli v0.7.28
error[E0599]: no method named `foo` found for struct `Bar`
   --> crates/soldr-cli/src/main.rs:42:5

error: could not compile `soldr-cli` (bin \"soldr\") due to 1 previous error
";
        assert_eq!(detect_build_script_failure(input), None);
    }

    #[test]
    fn returns_none_on_unrelated_build_script_panic() {
        // Build script panicked for a non-ENOENT reason (assertion,
        // missing env var, version mismatch, etc.). Don't mislead the
        // user with C-toolchain install hints when that wasn't the
        // problem.
        let input = "\
error: failed to run custom build command for `weird-crate v0.1.0`

Caused by:
  process didn't exit successfully: `build-script-build` (exit status: 101)
  --- stderr
  thread 'main' panicked at /home/x/weird-crate-0.1.0/build.rs:12:5:
  PROTOC env var must be set
";
        assert_eq!(detect_build_script_failure(input), None);
    }

    #[test]
    fn returns_none_for_empty_output() {
        assert_eq!(detect_build_script_failure(""), None);
        assert_eq!(detect_build_script_failure("\n\n"), None);
    }

    #[test]
    fn extracts_crate_name_with_complex_version_string() {
        // Pre-release / build-metadata semver suffixes (e.g. the
        // `+5.3.0` upstream-version tag tikv-jemalloc-sys uses).
        let input = "\
error: failed to run custom build command for `weird-name-sys v0.6.1+5.3.0`

Caused by:
  --- stderr
  thread 'main' panicked at /a/b/c/build.rs:1:1:
  failed to execute command: No such file or directory (os error 2)
";
        let diag = detect_build_script_failure(input).expect("must match");
        assert_eq!(diag.crate_name, "weird-name-sys");
    }

    #[test]
    fn ignores_running_line_outside_a_failure_block() {
        // A `running: "make"` line that appears BEFORE cargo's
        // `error: failed to run custom build command for` marker must
        // be ignored — it might just be a successful prior build.
        let input = "\
   Compiling foo-sys v1.0.0
   running: \"make\" \"-j\" \"4\"
   Compiling bar-sys v2.0.0

error: failed to run custom build command for `bar-sys v2.0.0`

Caused by:
  --- stderr
  thread 'main' panicked at /b/bar-sys-2.0.0/build.rs:99:9:
  failed to execute command: No such file or directory (os error 2)
";
        let diag = detect_build_script_failure(input).expect("must match");
        assert_eq!(diag.crate_name, "bar-sys");
        // No `running: ` line appeared INSIDE bar-sys's failure block,
        // so command_name should be None — not "make" from the
        // unrelated earlier block.
        assert!(diag.command_name.is_none());
    }

    #[test]
    fn second_failure_block_overrides_first_when_both_present() {
        // cargo `--keep-going` can report multiple custom-build
        // failures. The detector must scope `running:` extraction to
        // the most recent block.
        let input = "\
error: failed to run custom build command for `aaa v1.0.0`

Caused by:
  --- stdout
  running: \"first-tool\"
  --- stderr
  thread 'main' panicked at /a/build.rs:1:1:
  some unrelated panic

error: failed to run custom build command for `bbb v2.0.0`

Caused by:
  --- stdout
  running: \"second-tool\"
  --- stderr
  thread 'main' panicked at /b/build.rs:2:2:
  failed to execute command: No such file or directory (os error 2)
";
        let diag = detect_build_script_failure(input).expect("must match");
        assert_eq!(diag.crate_name, "bbb");
        assert_eq!(diag.command_name.as_deref(), Some("second-tool"));
    }

    #[test]
    fn rendered_diagnosis_lists_every_platform() {
        let diag = BuildScriptDiagnosis {
            crate_name: "tikv-jemalloc-sys".to_string(),
            command_name: Some("autogen.sh".to_string()),
            category: DiagnosisCategory::MissingHostTool,
        };
        let rendered = render_diagnosis(&diag);
        assert!(rendered.contains("tikv-jemalloc-sys"));
        assert!(rendered.contains("autogen.sh"));
        assert!(rendered.contains("debian/ubuntu"));
        assert!(rendered.contains("alpine"));
        assert!(rendered.contains("rhel/fedora"));
        assert!(rendered.contains("macos"));
        assert!(rendered.contains("windows"));
        assert!(rendered.contains("#422"));
    }

    #[test]
    fn rendered_diagnosis_omits_command_line_when_unknown() {
        let diag = BuildScriptDiagnosis {
            crate_name: "bzip2-sys".to_string(),
            command_name: None,
            category: DiagnosisCategory::MissingHostTool,
        };
        let rendered = render_diagnosis(&diag);
        assert!(rendered.contains("bzip2-sys"));
        assert!(
            !rendered.contains("failing command:"),
            "unknown command_name → omit the 'failing command' line; got:\n{rendered}",
        );
    }

    #[test]
    fn parses_running_line_with_path_separators() {
        // Build scripts commonly print full paths:
        //   running: "/usr/bin/make" "-C" "..."
        // We want the basename-ish form for the diagnosis. Today we
        // keep the full path so users can see exactly which exe cargo
        // tried to spawn — extracting basename is a cosmetic
        // improvement that can land later if it proves confusing.
        assert_eq!(
            parse_running_line("running: \"/usr/bin/make\" \"-C\" \"jemalloc\""),
            Some("/usr/bin/make".to_string()),
        );
        assert_eq!(
            parse_running_line("running: \"autogen.sh\" \"--prefix=/tmp\""),
            Some("autogen.sh".to_string()),
        );
        assert_eq!(parse_running_line("not a running line"), None);
        assert_eq!(parse_running_line("running: no quoted arg"), None);
    }

    #[test]
    fn extracts_crate_name_handles_path_dep_annotations() {
        // cargo decorates path deps in the failure marker:
        //   `error: failed to run custom build command for \`my-sys v1.0.0 (/abs/path)\``
        // Confirm we still pull just "my-sys".
        assert_eq!(
            parse_crate_name_from_marker(" `my-sys v1.0.0 (/abs/path/to/dep)`"),
            Some("my-sys".to_string()),
        );
    }
}
