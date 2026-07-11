//! Busybox-style argv[0] dispatch for shim identities.
//!
//! The release archive no longer ships `soldr-shim`,
//! `soldr-clang-shim`, or `zccache-soldr` sidecar binaries. Instead,
//! shim installers hardlink/copy the main `soldr` binary under each
//! shim name and this module selects the tiny fast path before
//! clap/tokio startup.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const TOOLCHAIN_TOOLS: &[&str] = &["cargo", "rustc", "rustfmt", "clippy-driver", "rustdoc"];
const ZCCACHE_SOLDR: &str = "zccache-soldr";
const SOLDR_DAEMON: &str = "soldr-daemon";

#[derive(Debug, PartialEq)]
pub(crate) enum MulticallDispatch {
    SoldrArgs(Vec<String>),
    Exit(i32),
    ExitCode(ExitCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShimIdentity {
    Toolchain(&'static str),
    Clang(ClangTool),
    ZccacheSoldr,
    SoldrDaemon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClangTool {
    Clang,
    ClangPP,
}

impl ClangTool {
    fn msvc_downstream(self) -> &'static str {
        "clang-cl"
    }

    fn passthrough_downstream(self) -> &'static str {
        match self {
            ClangTool::Clang => "clang",
            ClangTool::ClangPP => "clang++",
        }
    }
}

pub(crate) fn maybe_dispatch(raw_args: &[String]) -> Option<MulticallDispatch> {
    let argv0 = raw_args.first().map(String::as_str).unwrap_or("");
    match classify_argv0(argv0)? {
        ShimIdentity::Toolchain(tool) => {
            strip_self_from_path(argv0);
            let mut args = Vec::with_capacity(raw_args.len());
            args.push(tool.to_string());
            args.extend(raw_args.iter().skip(1).cloned());
            Some(MulticallDispatch::SoldrArgs(args))
        }
        ShimIdentity::Clang(tool) => Some(MulticallDispatch::Exit(run_clang_shim(tool))),
        ShimIdentity::ZccacheSoldr => Some(MulticallDispatch::Exit(run_zccache_soldr())),
        ShimIdentity::SoldrDaemon => Some(MulticallDispatch::Exit(crate::daemon_entry::run())),
    }
}

pub(crate) fn toolchain_shim_should_defer_to_rustc_wrapper(raw_args: &[String]) -> bool {
    let Some(rustc_path) = raw_args.get(1) else {
        return false;
    };
    if !is_rustc_wrapper_passthrough_tool(rustc_path) {
        return false;
    }
    let argv0 = raw_args.first().map(String::as_str).unwrap_or("");
    matches!(classify_argv0(argv0), Some(ShimIdentity::Toolchain(_)))
}

fn is_rustc_wrapper_passthrough_tool(arg: &str) -> bool {
    let stem = Path::new(arg)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(arg);
    matches!(stem, "rustc" | "clippy-driver")
}

fn classify_argv0(argv0: &str) -> Option<ShimIdentity> {
    let stem = argv0_stem(argv0)?;
    for &tool in TOOLCHAIN_TOOLS {
        if stem == tool {
            return Some(ShimIdentity::Toolchain(tool));
        }
    }
    match stem.as_str() {
        "clang" => Some(ShimIdentity::Clang(ClangTool::Clang)),
        "clang++" => Some(ShimIdentity::Clang(ClangTool::ClangPP)),
        ZCCACHE_SOLDR => Some(ShimIdentity::ZccacheSoldr),
        SOLDR_DAEMON => Some(ShimIdentity::SoldrDaemon),
        _ => None,
    }
}

fn argv0_stem(argv0: &str) -> Option<String> {
    let file_name = Path::new(argv0).file_name()?.to_string_lossy();
    let file_name = file_name.as_ref();
    let suffix_start = file_name.len().saturating_sub(4);
    let stem = if file_name
        .get(suffix_start..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".exe"))
    {
        file_name.get(..suffix_start).unwrap_or(file_name)
    } else {
        file_name
    };
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

fn strip_self_from_path(argv0: &str) {
    let Some(shim_dir) = current_shim_dir(argv0) else {
        return;
    };
    let Some(existing) = env::var_os("PATH") else {
        return;
    };
    let separator = if cfg!(windows) { ';' } else { ':' };
    let existing = existing.to_string_lossy();
    let new_path = filter_path(&existing, &shim_dir, separator);
    env::set_var("PATH", new_path);
}

fn current_shim_dir(argv0: &str) -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::parent)
        .map(canonicalize_or_self)
        .or_else(|| {
            Path::new(argv0)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(canonicalize_or_self)
        })
}

fn canonicalize_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn filter_path(existing: &str, shim_dir: &Path, separator: char) -> String {
    let shim_str = shim_dir.to_string_lossy();
    let mut out = String::with_capacity(existing.len());
    let mut first = true;
    for entry in existing.split(separator) {
        if entry.is_empty() {
            continue;
        }
        let matches = if cfg!(windows) {
            entry.eq_ignore_ascii_case(&shim_str)
        } else {
            entry == shim_str
        };
        if matches {
            continue;
        }
        if !first {
            out.push(separator);
        }
        out.push_str(entry);
        first = false;
    }
    out
}

fn run_zccache_soldr() -> i32 {
    let rustc_argv: Vec<String> = env::args().skip(1).collect();
    if rustc_argv.is_empty() {
        eprintln!(
            "zccache-soldr: missing compiler-path argument (wrapper contract is \
             `[wrapper_path, compiler_path, ...compiler_args]`)"
        );
        return 2;
    }

    match crate::compile_dispatch::dispatch_compile_detailed(
        &rustc_argv,
        std::io::stdout(),
        std::io::stderr(),
    ) {
        Ok(code) => normalize_exit_code(code),
        Err(failure) if crate::compile_dispatch::should_fall_back_to_direct_rustc(&failure) => {
            crate::compile_dispatch::log_direct_exec_fallback_once(&failure);
            match crate::compile_dispatch::direct_exec_rustc(&rustc_argv) {
                Ok(code) => normalize_exit_code(code),
                Err(err) => {
                    eprintln!("zccache-soldr: direct rustc fallback failed: {err}");
                    101
                }
            }
        }
        Err(failure) => {
            eprintln!(
                "zccache-soldr: dispatch failed: {}",
                failure.into_soldr_error()
            );
            101
        }
    }
}

fn run_clang_shim(tool: ClangTool) -> i32 {
    let argv: Vec<OsString> = env::args_os().collect();
    let args = argv.get(1..).unwrap_or(&[]);
    let downstream = if argv_has_msvc_target(args) {
        tool.msvc_downstream()
    } else {
        tool.passthrough_downstream()
    };

    match locate_in_path(downstream, true) {
        Some(path) => exec_with_argv(&path, args),
        None => {
            if downstream == "clang-cl" {
                eprintln!(
                    "clang shim: detected --target=*-pc-windows-msvc \
                     in argv but `clang-cl` is not on PATH"
                );
            } else {
                eprintln!(
                    "clang shim: passthrough mode but `{downstream}` \
                     is not on PATH after stripping the shim's own directory"
                );
            }
            127
        }
    }
}

fn argv_has_msvc_target(args: &[OsString]) -> bool {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let arg = arg.to_string_lossy();
        if let Some(triple) = arg.strip_prefix("--target=") {
            if triple.ends_with("-pc-windows-msvc") {
                return true;
            }
        }
        if arg == "--target" {
            if let Some(next) = iter.peek() {
                if next.to_string_lossy().ends_with("-pc-windows-msvc") {
                    return true;
                }
            }
        }
    }
    false
}

fn locate_in_path(binary: &str, strip_self: bool) -> Option<PathBuf> {
    let original_path = env::var_os("PATH").unwrap_or_default();
    let strip_dir = if strip_self {
        env::current_exe()
            .ok()
            .as_deref()
            .and_then(Path::parent)
            .map(canonicalize_or_self)
    } else {
        None
    };

    for dir in env::split_paths(&original_path) {
        if let Some(strip) = strip_dir.as_ref() {
            if canonicalize_or_self(&dir) == *strip {
                continue;
            }
        }
        let candidate = dir.join(exe_filename(binary));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(windows)]
fn exe_filename(binary: &str) -> String {
    format!("{binary}.exe")
}

#[cfg(not(windows))]
fn exe_filename(binary: &str) -> String {
    binary.to_string()
}

fn exec_with_argv(binary: &Path, args: &[OsString]) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(binary).args(args).exec();
        eprintln!("clang shim: exec({binary:?}) failed: {err}");
        126
    }
    #[cfg(windows)]
    {
        match std::process::Command::new(binary).args(args).status() {
            Ok(status) => status.code().map(normalize_exit_code).unwrap_or(1),
            Err(err) => {
                eprintln!("clang shim: spawn({binary:?}) failed: {err}");
                126
            }
        }
    }
}

fn normalize_exit_code(code: i32) -> i32 {
    if (0..=255).contains(&code) {
        code
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    crate::timed_test!(classify_argv0_recognizes_toolchain_shims, {
        assert_eq!(
            classify_argv0("/tmp/shims/cargo"),
            Some(ShimIdentity::Toolchain("cargo"))
        );
        assert_eq!(
            classify_argv0("clippy-driver.exe"),
            Some(ShimIdentity::Toolchain("clippy-driver"))
        );
        assert_eq!(classify_argv0("soldr"), None);
    });

    crate::timed_test!(classify_argv0_recognizes_clang_and_zccache_shims, {
        assert_eq!(
            classify_argv0("/tmp/clang-shim/clang"),
            Some(ShimIdentity::Clang(ClangTool::Clang))
        );
        assert_eq!(
            classify_argv0("clang++"),
            Some(ShimIdentity::Clang(ClangTool::ClangPP))
        );
        assert_eq!(
            classify_argv0("zccache-soldr"),
            Some(ShimIdentity::ZccacheSoldr)
        );
    });

    crate::timed_test!(classify_argv0_recognizes_runtime_aliases, {
        assert_eq!(
            classify_argv0("/opt/soldr/soldr-daemon"),
            Some(ShimIdentity::SoldrDaemon)
        );
        assert_eq!(
            classify_argv0("soldr-daemon.exe"),
            Some(ShimIdentity::SoldrDaemon)
        );
        assert_eq!(classify_argv0("zccache"), None);
        assert_eq!(classify_argv0("zccache.exe"), None);
    });

    crate::timed_test!(filter_path_drops_self_dir, {
        let shim = PathBuf::from("/opt/.soldr/v0.8/shims");
        let path = "/usr/bin:/opt/.soldr/v0.8/shims:/usr/local/bin";
        assert_eq!(filter_path(path, &shim, ':'), "/usr/bin:/usr/local/bin");
    });

    crate::timed_test!(argv_has_msvc_target_detects_single_and_split_forms, {
        assert!(argv_has_msvc_target(&os_args(&[
            "-O2",
            "--target=aarch64-pc-windows-msvc",
        ])));
        assert!(argv_has_msvc_target(&os_args(&[
            "--target",
            "x86_64-pc-windows-msvc",
        ])));
        assert!(!argv_has_msvc_target(&os_args(&[
            "--target=aarch64-unknown-linux-gnu",
        ])));
    });

    crate::timed_test!(normalize_exit_code_collapses_out_of_range_values, {
        assert_eq!(normalize_exit_code(0), 0);
        assert_eq!(normalize_exit_code(255), 255);
        assert_eq!(normalize_exit_code(256), 1);
        assert_eq!(normalize_exit_code(-1), 1);
    });

    crate::timed_test!(toolchain_shim_defers_rustc_wrapper_contract, {
        assert!(toolchain_shim_should_defer_to_rustc_wrapper(&[
            "/tmp/soldr-shims/cargo".into(),
            "/opt/rust/bin/rustc".into(),
            "-vV".into(),
        ]));
        assert!(toolchain_shim_should_defer_to_rustc_wrapper(&[
            "cargo.exe".into(),
            "clippy-driver.exe".into(),
            "-vV".into(),
        ]));
        assert!(!toolchain_shim_should_defer_to_rustc_wrapper(&[
            "/tmp/soldr-shims/cargo".into(),
            "build".into(),
        ]));
        assert!(!toolchain_shim_should_defer_to_rustc_wrapper(&[
            "zccache-soldr".into(),
            "/opt/rust/bin/rustc".into(),
            "-vV".into(),
        ]));
        assert!(!toolchain_shim_should_defer_to_rustc_wrapper(&[
            "clang".into(),
            "/opt/rust/bin/rustc".into(),
            "-vV".into(),
        ]));
    });
}
