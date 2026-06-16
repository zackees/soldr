//! `soldr-shim` — tiny multi-tool shim, dispatched by `argv[0]` basename.
//!
//! Installed by `soldr shims` (issue #742) into the versioned shim dir
//! `~/.soldr/v<X.Y.Z>/shims/` under each tool name (`cargo`, `rustc`,
//! `rustfmt`, `clippy-driver`, `rustdoc`). When a consumer (e.g.
//! `clud` per zackees/clud#343) prepends that dir to `PATH`, every
//! bare `cargo build` etc. resolves to this binary, which then:
//!
//! 1. Reads `argv[0]` and extracts its file-stem basename.
//! 2. Validates the basename is a known toolchain tool name.
//! 3. **Strips the shim's own parent directory from `PATH`** in its
//!    own process env so a nested `cargo` lookup inside soldr never
//!    re-hits the shim → no infinite recursion.
//! 4. Locates `soldr` via the modified `PATH`.
//! 5. Execs (POSIX) / spawns + waits (Windows) into
//!    `soldr <tool> <argv[1..]>` with the inherited modified env.
//!
//! Cold-start cost target: ~1ms. No tokio, no clap, no reqwest. The
//! whole point of the binary is that it's fast.

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const TOOLS: &[&str] = &["cargo", "rustc", "rustfmt", "clippy-driver", "rustdoc"];

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().collect();
    let argv0 = argv.first().map(String::as_str).unwrap_or("");
    let tool = match resolve_tool(argv0) {
        Some(t) => t,
        None => {
            eprintln!(
                "soldr-shim: unknown tool basename from argv[0]={argv0:?}; \
                 expected one of: {TOOLS:?}"
            );
            return ExitCode::from(2);
        }
    };

    let shim_path = PathBuf::from(argv0);
    if let Some(shim_dir) = shim_path.parent().and_then(canonicalize_or_none) {
        strip_self_from_path(&shim_dir);
    }

    let soldr_path = match locate_soldr() {
        Some(p) => p,
        None => {
            eprintln!("soldr-shim: could not locate `soldr` on PATH after self-strip");
            return ExitCode::from(127);
        }
    };

    delegate_to_soldr(&soldr_path, tool, &argv[1..])
}

/// Extract the tool name from `argv[0]`. Strips the platform exe
/// suffix (`.exe` on Windows) and validates against `TOOLS`.
fn resolve_tool(argv0: &str) -> Option<&'static str> {
    let path = Path::new(argv0);
    let stem = path.file_stem()?.to_str()?;
    TOOLS.iter().copied().find(|&t| t == stem)
}

/// Best-effort canonicalize (resolves symlinks). On failure return the
/// path as-is so PATH-strip still has SOMETHING to compare against.
fn canonicalize_or_none(p: &Path) -> Option<PathBuf> {
    if let Ok(real) = p.canonicalize() {
        return Some(real);
    }
    Some(p.to_path_buf())
}

/// Remove `shim_dir` from `PATH` in this process's env. Case-insensitive
/// equality on Windows, byte-equal elsewhere. Entries that don't equal
/// the shim_dir (and non-empty entries) are preserved in their original
/// order.
pub fn strip_self_from_path(shim_dir: &Path) {
    let Some(existing) = env::var_os("PATH") else {
        return;
    };
    let existing_str = existing.to_string_lossy();
    let separator = if cfg!(windows) { ';' } else { ':' };
    let new_path = filter_path(&existing_str, shim_dir, separator);
    // SAFETY: shim binary is single-threaded at this point; no other
    // thread is reading PATH. set_var is safe.
    unsafe {
        env::set_var("PATH", new_path);
    }
}

/// Pure helper for `strip_self_from_path` — exposed for unit testing.
pub fn filter_path(existing: &str, shim_dir: &Path, separator: char) -> String {
    let shim_str = shim_dir.to_string_lossy();
    let mut out = String::with_capacity(existing.len());
    let mut first = true;
    for entry in existing.split(separator) {
        if entry.is_empty() {
            continue;
        }
        let eq = if cfg!(windows) {
            entry.eq_ignore_ascii_case(&shim_str)
        } else {
            entry == shim_str
        };
        if eq {
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

/// Walk the (already-modified) `PATH` looking for `soldr` /
/// `soldr.exe`. We don't use `execvp` semantics because Windows lacks
/// a portable equivalent that respects an in-process env change.
fn locate_soldr() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    let exe_name = if cfg!(windows) { "soldr.exe" } else { "soldr" };
    for dir in env::split_paths(&path) {
        let candidate = dir.join(exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// POSIX: replace this process with `soldr <tool> argv[1..]`. Windows:
/// spawn + wait + propagate exit code.
fn delegate_to_soldr(soldr: &Path, tool: &str, rest: &[String]) -> ExitCode {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // execvp-like: replace current process. Builds child argv with
        // soldr's basename as argv[0] (matches what `soldr` would see if
        // invoked directly via PATH).
        let err = std::process::Command::new(soldr)
            .arg(tool)
            .args(rest)
            .exec();
        eprintln!("soldr-shim: failed to exec `{}`: {err}", soldr.display());
        ExitCode::from(127)
    }
    #[cfg(windows)]
    {
        // Windows: no execvp. Spawn the child, wait, propagate exit code.
        // PATH was modified above; CreateProcess (via std::process)
        // inherits the modified env automatically.
        let status = std::process::Command::new(soldr)
            .arg(tool)
            .args(rest)
            .status();
        match status {
            Ok(s) => {
                let code = s.code().unwrap_or(1);
                if !(0..=255).contains(&code) {
                    ExitCode::from(1)
                } else {
                    ExitCode::from(code as u8)
                }
            }
            Err(err) => {
                eprintln!("soldr-shim: failed to spawn `{}`: {err}", soldr.display());
                ExitCode::from(127)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The binary uses these unit tests to verify pure-helper behavior.
    // Cross-OS scenarios are covered via `#[cfg(...)]` attributes since
    // the comparison semantics differ between Windows and POSIX.

    #[test]
    fn resolve_tool_recognizes_each_canonical_name() {
        for tool in TOOLS {
            assert_eq!(resolve_tool(tool), Some(*tool), "should resolve {tool}");
        }
    }

    #[test]
    fn resolve_tool_recognizes_with_exe_suffix() {
        assert_eq!(resolve_tool("cargo.exe"), Some("cargo"));
        assert_eq!(resolve_tool("rustfmt.exe"), Some("rustfmt"));
        assert_eq!(resolve_tool("clippy-driver.exe"), Some("clippy-driver"));
    }

    #[test]
    fn resolve_tool_recognizes_with_full_path() {
        assert_eq!(
            resolve_tool("/home/user/.soldr/v0.7.55/shims/cargo"),
            Some("cargo")
        );
        assert_eq!(
            resolve_tool(r"C:\Users\u\.soldr\v0.7.55\shims\rustc.exe"),
            Some("rustc")
        );
    }

    #[test]
    fn resolve_tool_rejects_unknown() {
        assert_eq!(resolve_tool("ls"), None);
        assert_eq!(resolve_tool(""), None);
        assert_eq!(resolve_tool("cargo-nextest"), None); // not a toolchain tool
    }

    #[cfg(unix)]
    #[test]
    fn filter_path_drops_self_dir_byte_equal_on_unix() {
        let shim = PathBuf::from("/opt/.soldr/v0.7.55/shims");
        let path = "/usr/bin:/opt/.soldr/v0.7.55/shims:/usr/local/bin";
        let out = filter_path(path, &shim, ':');
        assert_eq!(out, "/usr/bin:/usr/local/bin");
    }

    #[cfg(unix)]
    #[test]
    fn filter_path_case_sensitive_on_unix() {
        // /Soldr (cap S) is a DIFFERENT path from /soldr on POSIX —
        // must NOT be stripped.
        let shim = PathBuf::from("/opt/.soldr/shims");
        let path = "/opt/.Soldr/shims:/usr/bin";
        let out = filter_path(path, &shim, ':');
        assert_eq!(out, "/opt/.Soldr/shims:/usr/bin");
    }

    #[cfg(windows)]
    #[test]
    fn filter_path_drops_self_dir_case_insensitive_on_windows() {
        let shim = PathBuf::from(r"C:\Users\u\.soldr\v0.7.55\shims");
        let path = r"C:\Windows\System32;c:\users\u\.SOLDR\v0.7.55\SHIMS;C:\Windows";
        let out = filter_path(path, &shim, ';');
        assert_eq!(out, r"C:\Windows\System32;C:\Windows");
    }

    #[test]
    fn filter_path_preserves_order_and_non_matching_entries() {
        let shim = PathBuf::from("/middle/shims");
        let sep = if cfg!(windows) { ';' } else { ':' };
        let entries = ["/a/bin", "/middle/shims", "/b/bin", "/c/bin"];
        let path = entries.join(&sep.to_string());
        let out = filter_path(&path, &shim, sep);
        let expected = ["/a/bin", "/b/bin", "/c/bin"].join(&sep.to_string());
        // case-insensitive on Windows matters for the equality, not the order.
        if cfg!(windows) {
            assert!(
                out.eq_ignore_ascii_case(&expected),
                "expected order preserved (case-insensitive): out={out}"
            );
        } else {
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn filter_path_drops_empty_entries() {
        let shim = PathBuf::from("/never");
        let sep = if cfg!(windows) { ';' } else { ':' };
        let path = format!("/a{sep}{sep}/b");
        let out = filter_path(&path, &shim, sep);
        assert_eq!(out, format!("/a{sep}/b"));
    }
}
