//! Standalone catalogue-backed C/C++ compiler driver (soldr#2335).
//!
//! `soldr cc` and `soldr c++` deliberately prepare only the native compiler
//! toolchain. Routing through `target_lifecycle::prepare_target` would also
//! install Rust std targets, provision CMake, and probe every `*-sys` library
//! override on every compiler process CMake starts.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct CcArgs {
    /// Compilation target (friendly alias or Rust triple, optionally `.2.17`)
    #[arg(long, default_value = "host", value_name = "TARGET")]
    pub(crate) target: String,

    /// Print the prepared C compiler path instead of invoking it
    #[arg(long, group = "print_tool")]
    pub(crate) print_cc: bool,

    /// Print the prepared C++ compiler path instead of invoking it
    #[arg(long, group = "print_tool")]
    pub(crate) print_cxx: bool,

    /// Print the prepared archiver path instead of invoking the compiler
    #[arg(long, group = "print_tool")]
    pub(crate) print_ar: bool,

    /// Print the prepared linker-driver path instead of invoking the compiler
    #[arg(long, group = "print_tool")]
    pub(crate) print_linker: bool,

    /// Arguments forwarded verbatim to the selected compiler
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    C,
    Cxx,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeToolchain {
    cc: PathBuf,
    cxx: PathBuf,
    ar: PathBuf,
    linker: PathBuf,
    bin_dir: PathBuf,
    compiler_args: Vec<String>,
}

pub(crate) async fn run(args: CcArgs, language: Language) -> Result<i32, SoldrError> {
    if requests_print(&args) && !args.args.is_empty() {
        return Err(SoldrError::Other(format!(
            "soldr {}: compiler arguments cannot be combined with --print-*",
            language.command_name()
        )));
    }

    let target = normalize_driver_target(&args.target);
    let resolved = crate::target_alias::resolve_soldr_target(&target).map_err(|error| {
        SoldrError::Other(reword_alias_error(
            error.to_string(),
            language.command_name(),
        ))
    })?;
    let paths = SoldrPaths::new()?;
    let tools = prepare_native_toolchain(&paths, &resolved.rust_triple).await?;

    if let Some(path) = selected_print_path(&args, &tools) {
        println!("{}", path.display());
        return Ok(0);
    }

    let compiler = match language {
        Language::C => &tools.cc,
        Language::Cxx => &tools.cxx,
    };
    let mut command = Command::new(compiler);
    command.args(&tools.compiler_args);
    command.args(&args.args);
    command.env("PATH", path_with_prepend(&tools.bin_dir)?);
    suppress_windows_console_window(&mut command);

    let status = command.status().map_err(|error| {
        SoldrError::Other(format!(
            "soldr {}: failed to start compiler {} for target {}: {error}",
            language.command_name(),
            compiler.display(),
            resolved.rust_triple
        ))
    })?;
    Ok(status.code().unwrap_or(1))
}

fn requests_print(args: &CcArgs) -> bool {
    args.print_cc || args.print_cxx || args.print_ar || args.print_linker
}

impl Language {
    fn command_name(self) -> &'static str {
        match self {
            Self::C => "cc",
            Self::Cxx => "c++",
        }
    }
}

fn selected_print_path<'a>(args: &CcArgs, tools: &'a NativeToolchain) -> Option<&'a Path> {
    if args.print_cc {
        Some(&tools.cc)
    } else if args.print_cxx {
        Some(&tools.cxx)
    } else if args.print_ar {
        Some(&tools.ar)
    } else if args.print_linker {
        Some(&tools.linker)
    } else {
        None
    }
}

async fn prepare_native_toolchain(
    paths: &SoldrPaths,
    target: &str,
) -> Result<NativeToolchain, SoldrError> {
    if target.ends_with("-unknown-linux-gnu") {
        if !cfg!(target_os = "linux") {
            return Err(unsupported_host(target));
        }
        let toolchain = crate::fetch::gnu_linux_toolchain::ensure(paths, target).await?;
        return Ok(NativeToolchain {
            cc: toolchain.tool_path("gcc"),
            cxx: toolchain.tool_path("g++"),
            ar: toolchain.tool_path("ar"),
            linker: toolchain.tool_path("gcc"),
            bin_dir: toolchain.bin_dir,
            compiler_args: vec![format!("--sysroot={}", toolchain.sysroot.display())],
        });
    }

    if target.ends_with("-unknown-linux-musl") {
        if !cfg!(target_os = "linux") {
            return Err(unsupported_host(target));
        }
        let toolchain = crate::fetch::musl_linux_toolchain::ensure(paths, target).await?;
        return Ok(NativeToolchain {
            cc: toolchain.tool_path("gcc"),
            cxx: toolchain.tool_path("g++"),
            ar: toolchain.tool_path("ar"),
            linker: toolchain.tool_path("gcc"),
            bin_dir: toolchain.bin_dir,
            compiler_args: vec![format!("--sysroot={}", toolchain.sysroot.display())],
        });
    }

    if target == crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_TARGET {
        let (bin_dir, env) =
            crate::fetch::mingw_w64_gcc::prepare_win_gnu_env(paths, target).await?;
        let suffix = target.replace('-', "_");
        let upper = suffix.to_ascii_uppercase();
        return Ok(NativeToolchain {
            cc: env_path(&env, &format!("CC_{suffix}"))?,
            cxx: env_path(&env, &format!("CXX_{suffix}"))?,
            ar: env_path(&env, &format!("AR_{suffix}"))?,
            linker: env_path(&env, &format!("CARGO_TARGET_{upper}_LINKER"))?,
            bin_dir,
            compiler_args: Vec::new(),
        });
    }

    Err(SoldrError::UnsupportedPlatform(format!(
        "soldr C/C++ does not yet expose a standalone compiler for `{target}`; \
         the initial soldr#2335 surface supports GNU/Linux, musl/Linux, and \
         x86_64-pc-windows-gnu targets"
    )))
}

fn env_path(env: &[(String, String)], key: &str) -> Result<PathBuf, SoldrError> {
    env.iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| PathBuf::from(value))
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "soldr C/C++: prepared toolchain did not provide required `{key}`"
            ))
        })
}

fn unsupported_host(target: &str) -> SoldrError {
    SoldrError::UnsupportedPlatform(format!(
        "the catalogue compiler for `{target}` is a Linux executable; \
         standalone `soldr cc`/`soldr c++` for this host/target pair is tracked in soldr#2319"
    ))
}

fn path_with_prepend(dir: &Path) -> Result<std::ffi::OsString, SoldrError> {
    let mut entries = vec![dir.to_path_buf()];
    if let Some(path) = std::env::var_os("PATH") {
        entries.extend(std::env::split_paths(&path).filter(|entry| entry != dir));
    }
    std::env::join_paths(entries)
        .map_err(|error| SoldrError::Other(format!("soldr C/C++: failed to prepare PATH: {error}")))
}

fn reword_alias_error(message: String, command_name: &str) -> String {
    message.replacen(
        "soldr build --target",
        &format!("soldr {command_name} --target"),
        1,
    )
}

/// Accept the concise compiler-driver spellings used by GCC/Zig in addition
/// to Soldr's Rust triples. The target lifecycle remains keyed by canonical
/// Rust triples after this boundary.
fn normalize_driver_target(input: &str) -> String {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    for (driver, rust) in [
        ("x86_64-linux-gnu", "x86_64-unknown-linux-gnu"),
        ("aarch64-linux-gnu", "aarch64-unknown-linux-gnu"),
        ("x86_64-linux-musl", "x86_64-unknown-linux-musl"),
        ("aarch64-linux-musl", "aarch64-unknown-linux-musl"),
    ] {
        if lower == driver {
            return rust.to_string();
        }
        if let Some(suffix) = lower.strip_prefix(&format!("{driver}.")) {
            return format!("{rust}.{suffix}");
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(print_tool_selection_is_unambiguous, {
        let tools = NativeToolchain {
            cc: PathBuf::from("/sdk/cc"),
            cxx: PathBuf::from("/sdk/c++"),
            ar: PathBuf::from("/sdk/ar"),
            linker: PathBuf::from("/sdk/linker"),
            bin_dir: PathBuf::from("/sdk"),
            compiler_args: Vec::new(),
        };
        let args = CcArgs {
            target: "host".to_string(),
            print_cc: false,
            print_cxx: true,
            print_ar: false,
            print_linker: false,
            args: Vec::new(),
        };
        assert_eq!(
            selected_print_path(&args, &tools),
            Some(Path::new("/sdk/c++"))
        );
    });

    crate::timed_test!(alias_errors_name_the_cc_surface, {
        let error = crate::target_alias::resolve_soldr_target("not-target")
            .unwrap_err()
            .to_string();
        let message = reword_alias_error(error, "cc");
        assert!(message.starts_with("soldr cc --target"));
        assert!(!message.starts_with("soldr build --target"));
    });

    crate::timed_test!(print_request_with_compiler_args_is_detectable_early, {
        let args = CcArgs {
            target: "host".to_string(),
            print_cc: true,
            print_cxx: false,
            print_ar: false,
            print_linker: false,
            args: vec!["hello.c".to_string()],
        };
        assert!(requests_print(&args));
        assert!(!args.args.is_empty());
        assert!(
            reword_alias_error("soldr build --target: bad target".to_string(), "c++")
                .starts_with("soldr c++ --target")
        );
    });

    crate::timed_test!(zig_style_gnu_target_normalizes_to_the_rust_triple, {
        assert_eq!(
            normalize_driver_target("x86_64-linux-gnu.2.17"),
            "x86_64-unknown-linux-gnu.2.17"
        );
        assert_eq!(
            normalize_driver_target("aarch64-linux-gnu"),
            "aarch64-unknown-linux-gnu"
        );
    });

    crate::timed_test!(path_prefix_keeps_the_managed_bin_first, {
        let _env = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (managed, host, other) = if cfg!(windows) {
            (r"C:\managed\bin", r"C:\host\bin", r"C:\other")
        } else {
            ("/managed/bin", "/host/bin", "/other")
        };
        let _path = crate::EnvVarGuard::set("PATH", std::env::join_paths([host, other]).unwrap());
        let value = path_with_prepend(Path::new(managed)).unwrap();
        let entries = std::env::split_paths(&value).collect::<Vec<_>>();
        assert_eq!(entries.first(), Some(&PathBuf::from(managed)));
    });
}
