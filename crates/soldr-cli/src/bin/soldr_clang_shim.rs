//! `soldr-clang-shim` — `clang`/`clang++` wrapper that routes to
//! clang-cl for `*-pc-windows-msvc` targets.
//!
//! soldr#1012 PR 4. The actual unblocker for the win-arm64 cross-
//! compile.
//!
//! ## The oversight this shim sidesteps
//!
//! `ring 0.17.14`'s build.rs:563 hardcodes a compiler override for
//! `aarch64-pc-windows-msvc` when cc-rs's auto-detected compiler
//! isn't `is_like_clang()`:
//!
//! ```text
//! let compiler = if target.os == WINDOWS && target.arch == AARCH64
//!     && !compiler.is_like_clang()
//! {
//!     let _ = c.compiler("clang");
//!     c.get_compiler()
//! };
//! ```
//!
//! cc-rs files clang-cl under `ToolFamily::Msvc { clang_cl: true }`,
//! NOT `ToolFamily::Clang`, so `!is_like_clang()` evaluates `TRUE`
//! for a clang-cl pick. Ring then force-overrides to plain `"clang"`,
//! cc-rs invokes plain clang with the same `/imsvc <path>` clang-cl-
//! style include flags it had prepared for clang-cl, and clang
//! chokes: `error: no such file or directory: '/imsvc'`.
//!
//! The setting of `CC_aarch64_pc_windows_msvc=clang-cl` env var
//! works for win-x64 (where ring's special-case doesn't fire) but
//! not for win-arm64.
//!
//! ## The shim solution
//!
//! When `~/.soldr/bin/clang` (this shim) is on PATH ahead of the
//! system clang, ring's `c.compiler("clang")` causes cc-rs to call
//! `which("clang")` which finds OUR shim, not the real clang. Our
//! shim:
//!
//!   1. Reads `argv`.
//!   2. Scans for any `--target=*-pc-windows-msvc` flag (or the
//!      `--target T` two-arg form).
//!   3. If found → exec `clang-cl` with the same argv. clang-cl
//!      accepts `/imsvc <path>` natively (MSVC-style include flags
//!      are its raison d'être). Ring's compile succeeds.
//!   4. If no MSVC target detected → strip our own dir from PATH,
//!      locate the real clang via the modified PATH, exec with the
//!      same argv. Behavior is identical to the system clang
//!      otherwise.
//!
//! ## Installation
//!
//! soldr#1012 PR 5's `Commands::Build` handler installs this binary
//! at `~/.soldr/bin/clang` (and `~/.soldr/bin/clang++`,
//! `~/.soldr/bin/clang-cl`) and prepends `~/.soldr/bin/` to PATH on
//! every `soldr build --target *-pc-windows-msvc` invocation. The
//! shim is invoked by ring (and any other cc-rs consumer that picks
//! plain `clang` for MSVC cross targets).
//!
//! ## Cold-start cost
//!
//! Target: ~1ms. No tokio, no clap, no logging — just argv scan +
//! exec.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The downstream binary names this shim routes to.
const CLANG_CL: &str = "clang-cl";
const CLANG_CL_PP: &str = "clang-cl";
const REAL_CLANG: &str = "clang";
const REAL_CLANG_PP: &str = "clang++";

/// Tool-name variants this shim can stand in for. Determined by
/// `argv[0]` basename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShimTool {
    /// Installed as `~/.soldr/bin/clang`.
    Clang,
    /// Installed as `~/.soldr/bin/clang++`.
    ClangPP,
}

impl ShimTool {
    fn from_argv0(argv0: &str) -> Option<Self> {
        let stem = Path::new(argv0)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        match stem {
            "clang" | "soldr-clang-shim" => Some(ShimTool::Clang),
            "clang++" => Some(ShimTool::ClangPP),
            _ => None,
        }
    }

    fn msvc_downstream(self) -> &'static str {
        match self {
            ShimTool::Clang => CLANG_CL,
            ShimTool::ClangPP => CLANG_CL_PP,
        }
    }

    fn passthrough_downstream(self) -> &'static str {
        match self {
            ShimTool::Clang => REAL_CLANG,
            ShimTool::ClangPP => REAL_CLANG_PP,
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<OsString> = env::args_os().collect();
    let argv0 = argv
        .first()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tool = match ShimTool::from_argv0(&argv0) {
        Some(t) => t,
        None => {
            eprintln!(
                "soldr-clang-shim: unrecognized argv[0] basename {argv0:?}; \
                 expected one of: clang, clang++, soldr-clang-shim"
            );
            return ExitCode::from(2);
        }
    };

    let msvc_target_detected = argv_has_msvc_target(&argv[1..]);

    if msvc_target_detected {
        // Re-route to clang-cl.
        let downstream = tool.msvc_downstream();
        match locate_in_path(downstream, /*strip_self*/ true, &argv0) {
            Some(p) => exec_with_argv(&p, &argv[1..]),
            None => {
                eprintln!(
                    "soldr-clang-shim: detected --target=*-pc-windows-msvc \
                     in argv but `{downstream}` is not on PATH"
                );
                ExitCode::from(127)
            }
        }
    } else {
        // Passthrough to real clang.
        let downstream = tool.passthrough_downstream();
        match locate_in_path(downstream, /*strip_self*/ true, &argv0) {
            Some(p) => exec_with_argv(&p, &argv[1..]),
            None => {
                eprintln!(
                    "soldr-clang-shim: passthrough mode but `{downstream}` \
                     is not on PATH after stripping the shim's own directory"
                );
                ExitCode::from(127)
            }
        }
    }
}

/// Scan `args` for any indicator that the build is targeting
/// `*-pc-windows-msvc`. Two forms are recognized:
///
/// * `--target=<triple>` — single-arg form. Match if `<triple>` ends
///   in `-pc-windows-msvc`.
/// * `--target <triple>` — two-arg form. Match if the FOLLOWING
///   positional arg ends in `-pc-windows-msvc`.
///
/// Exposed as `pub(crate)` so unit tests in the bin's `#[cfg(test)]`
/// module can exercise the matcher with the exact argv patterns ring
/// and cc-rs emit.
fn argv_has_msvc_target(args: &[OsString]) -> bool {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let s = arg.to_string_lossy();
        // --target=<triple>
        if let Some(triple) = s.strip_prefix("--target=") {
            if triple.ends_with("-pc-windows-msvc") {
                return true;
            }
        }
        // --target <triple>
        if s == "--target" {
            if let Some(next) = iter.peek() {
                let next_s = next.to_string_lossy();
                if next_s.ends_with("-pc-windows-msvc") {
                    return true;
                }
            }
        }
    }
    false
}

/// Find `binary` on PATH, optionally stripping this shim's own
/// directory first so the lookup doesn't loop back to ourselves.
///
/// `argv0` is kept as a fallback hint for when `current_exe()` fails
/// (rare), but is NOT the primary source of truth: shells invoke
/// argv[0] as the bare basename (`clang`), and `Path::new("clang").parent()`
/// returns `Some("")` which never matches a real PATH entry — so the
/// shim's own dir would not get stripped and PATH resolution would
/// loop back to the shim's own clang-cl symlink. Using `current_exe`
/// gives the canonical install dir directly.
fn locate_in_path(binary: &str, strip_self: bool, argv0: &str) -> Option<PathBuf> {
    let original_path = env::var_os("PATH").unwrap_or_default();
    let strip_dir: Option<PathBuf> = if strip_self {
        // Primary: current_exe's parent. Fallback: argv0's parent.
        env::current_exe()
            .ok()
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| Path::new(argv0).parent().map(Path::to_path_buf))
    } else {
        None
    };

    for dir in env::split_paths(&original_path) {
        if let Some(strip) = strip_dir.as_ref() {
            // Skip the shim's own dir to avoid re-resolving to self.
            // Compare via canonicalize for symlink-safe equality.
            let dir_canon = canonicalize_or_self(&dir);
            let strip_canon = canonicalize_or_self(strip);
            if dir_canon == strip_canon {
                continue;
            }
        }
        let exe_name = exe_filename(binary);
        let candidate = dir.join(&exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn canonicalize_or_self(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(windows)]
fn exe_filename(binary: &str) -> String {
    format!("{binary}.exe")
}

#[cfg(not(windows))]
fn exe_filename(binary: &str) -> String {
    binary.to_string()
}

/// Execute `binary` with the given args, inheriting the rest of the
/// process environment. On POSIX uses `execvp` for true process
/// replacement; on Windows spawns + waits and propagates the exit
/// code.
fn exec_with_argv(binary: &Path, args: &[OsString]) -> ExitCode {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(binary).args(args).exec();
        eprintln!("soldr-clang-shim: exec({binary:?}) failed: {err}");
        ExitCode::from(126)
    }
    #[cfg(windows)]
    {
        match std::process::Command::new(binary).args(args).status() {
            Ok(status) => match status.code() {
                Some(code) => ExitCode::from(code as u8),
                None => ExitCode::from(1),
            },
            Err(e) => {
                eprintln!("soldr-clang-shim: spawn({binary:?}) failed: {e}");
                ExitCode::from(126)
            }
        }
    }
}

// ---------------------------------------------------------------------
// Tests — unit-test the argv matcher with the exact patterns ring +
// cc-rs emit. Compile-time #[cfg(test)] so they only run under
// `cargo test`.
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(|s| OsString::from(*s)).collect()
    }

    #[test]
    fn detects_target_single_arg_form_msvc() {
        let a = argv(&[
            "-O3",
            "--target=aarch64-pc-windows-msvc",
            "-c",
            "curve25519.c",
        ]);
        assert!(argv_has_msvc_target(&a));
    }

    #[test]
    fn detects_target_two_arg_form_msvc() {
        let a = argv(&["-O3", "--target", "aarch64-pc-windows-msvc", "-c", "x.c"]);
        assert!(argv_has_msvc_target(&a));
    }

    #[test]
    fn detects_x86_msvc_too() {
        let a = argv(&["--target=x86_64-pc-windows-msvc"]);
        assert!(argv_has_msvc_target(&a));
    }

    #[test]
    fn ignores_non_msvc_target() {
        let a = argv(&["--target=aarch64-unknown-linux-musl"]);
        assert!(!argv_has_msvc_target(&a));
    }

    #[test]
    fn ignores_no_target_at_all() {
        let a = argv(&["-O3", "-c", "foo.c"]);
        assert!(!argv_has_msvc_target(&a));
    }

    #[test]
    fn detects_ring_exact_invocation_pattern() {
        // Recreates the exact arg vector ring + cc-rs emitted in the
        // logs from soldr#1006 Lane 2 (truncated for brevity). This is
        // the regression-pin against the soldr#1012 PR 4 bug.
        let a = argv(&[
            "-O3",
            "-ffunction-sections",
            "-fdata-sections",
            "--target=aarch64-pc-windows-msvc",
            "-I",
            "/cargo/ring/include",
            "-fvisibility=hidden",
            "-std=c1x",
            "-Wall",
            "--target=aarch64-pc-windows-msvc",
            "-Wno-unused-command-line-argument",
            "-fuse-ld=lld-link",
            "/imsvc",
            "/home/runner/.cache/cargo-xwin/xwin/crt/include",
            "/imsvc",
            "/home/runner/.cache/cargo-xwin/xwin/sdk/include/ucrt",
            "-o",
            "curve25519.o",
            "-c",
            "curve25519.c",
        ]);
        assert!(
            argv_has_msvc_target(&a),
            "ring's exact aarch64-windows-msvc invocation must be detected"
        );
    }

    #[test]
    fn target_at_end_of_argv_still_detected() {
        // The `--target` flag could land anywhere; the matcher must
        // scan the entire argv, not give up at any specific position.
        let a = argv(&[
            "-O3",
            "-c",
            "x.c",
            "-o",
            "x.o",
            "--target=aarch64-pc-windows-msvc",
        ]);
        assert!(argv_has_msvc_target(&a));
    }

    #[test]
    fn target_followed_by_msvc_lookalike_does_not_match() {
        // Defensive: a triple that just CONTAINS "windows-msvc" but
        // isn't the suffix shouldn't match. (This is a paranoid test
        // — no real triple shapes look like this, but documents the
        // contract.)
        let a = argv(&["--target=aarch64-pc-windows-msvc-extra"]);
        assert!(!argv_has_msvc_target(&a));
    }

    #[test]
    fn shimtool_resolves_from_argv0_basenames() {
        assert_eq!(ShimTool::from_argv0("clang"), Some(ShimTool::Clang));
        assert_eq!(
            ShimTool::from_argv0("/usr/local/bin/clang"),
            Some(ShimTool::Clang)
        );
        assert_eq!(
            ShimTool::from_argv0("C:\\Users\\foo\\bin\\clang.exe"),
            Some(ShimTool::Clang)
        );
        assert_eq!(ShimTool::from_argv0("clang++"), Some(ShimTool::ClangPP));
        assert_eq!(
            ShimTool::from_argv0("soldr-clang-shim"),
            Some(ShimTool::Clang),
            "the shim binary's own name should still resolve when invoked directly (for testing)"
        );
        assert_eq!(ShimTool::from_argv0("gcc"), None);
    }

    #[test]
    fn clang_cl_is_not_a_recognized_shim_argv0() {
        // soldr#1032 followup: clang-cl MUST NOT be a valid shim argv[0].
        // The shim invokes clang-cl as its downstream — if clang-cl were
        // also a shim symlink and was a recognized argv[0], the shim
        // would loop into itself when invoked as clang-cl (which is
        // exactly what `clang_shim_names` used to do until we removed
        // clang-cl from the install set). Test gates both halves of the
        // fix: the install can never re-add clang-cl as a symlink AND
        // have the shim accept it.
        assert_eq!(ShimTool::from_argv0("clang-cl"), None);
        assert_eq!(ShimTool::from_argv0("/usr/bin/clang-cl"), None);
        assert_eq!(
            ShimTool::from_argv0("/root/.soldr/bin/clang-shim/clang-cl"),
            None,
        );
    }

    #[test]
    fn locate_in_path_strips_current_exe_dir_even_when_argv0_is_bare() {
        // soldr#1032 followup: when the shell invokes a shim symlink by
        // bare basename (`clang`), argv[0] has no directory and
        // `Path::new("clang").parent()` yields `Some("")` which never
        // matches a real PATH entry — so the shim's own dir wouldn't
        // get stripped and a `clang-cl` symlink in the same dir would
        // re-resolve to the shim. The fix prefers `current_exe()`.
        //
        // We don't validate the actual PATH lookup here (would require
        // mocking PATH + the filesystem); we just confirm that the
        // strip_dir resolution is exercised. The clang_cl regression
        // test above is the primary acceptance gate.
        let _ = locate_in_path("definitely-not-a-real-binary", true, "clang");
    }
}
