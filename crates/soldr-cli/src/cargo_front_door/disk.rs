//! Low-disk warning emission, free-space probing, and the small
//! path/argv helpers shared between the disk probe and PATH wiring.
//!
//! `available_space` and `existing_filesystem_probe_path` are also
//! consumed by `crate::gc`, so they stay `pub(crate)` rather than
//! `pub(super)`.

use crate::core::SoldrError;
use crate::LOW_DISK_WARNING_THRESHOLD_BYTES;
use crate::TEST_FREE_DISK_BYTES_ENV_VAR;

pub(super) fn maybe_emit_low_disk_warning(path: &std::path::Path) {
    if let Some(message) =
        low_disk_warning_for_path(path, stderr_should_use_color(), available_space)
    {
        eprintln!("{message}");
    }
}

pub(crate) fn low_disk_warning_for_path<F>(
    path: &std::path::Path,
    use_color: bool,
    available_space: F,
) -> Option<String>
where
    F: FnOnce(&std::path::Path) -> std::io::Result<u64>,
{
    let probe_path = existing_filesystem_probe_path(path);
    let free_bytes = available_space(&probe_path).ok()?;
    low_disk_warning_for_free_bytes(free_bytes, use_color)
}

pub(crate) fn low_disk_warning_for_free_bytes(free_bytes: u64, use_color: bool) -> Option<String> {
    if free_bytes >= LOW_DISK_WARNING_THRESHOLD_BYTES {
        return None;
    }
    let warning = if use_color {
        "\x1b[33mwarning\x1b[0m"
    } else {
        "warning"
    };
    Some(format!(
        "soldr: {warning}: disk space is low ({} free). Run `soldr gc` to review reclaimable Rust target directories.",
        crate::cache_lib::target_registry::human_size(free_bytes),
    ))
}

fn stderr_should_use_color() -> bool {
    use std::io::IsTerminal;

    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

pub(crate) fn available_space(path: &std::path::Path) -> std::io::Result<u64> {
    if let Some(raw) = std::env::var_os(TEST_FREE_DISK_BYTES_ENV_VAR) {
        let raw = raw.to_string_lossy();
        if raw.eq_ignore_ascii_case("error") {
            return Err(std::io::Error::other("test disk-space failure"));
        }
        return raw.parse::<u64>().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid {TEST_FREE_DISK_BYTES_ENV_VAR}: {e}"),
            )
        });
    }
    fs2::available_space(path)
}

pub(crate) fn existing_filesystem_probe_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut cursor = if path.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    } else {
        path.to_path_buf()
    };
    loop {
        if cursor.exists() {
            return cursor;
        }
        if !cursor.pop() {
            return std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        }
    }
}

pub(super) fn cargo_disk_space_probe_path(args: &[String]) -> std::path::PathBuf {
    if let Some(target_dir) = cargo_arg_value(args, "--target-dir") {
        return absolutize_path(std::path::PathBuf::from(target_dir));
    }
    if let Some(target_dir) = crate::non_empty_env_path("CARGO_TARGET_DIR") {
        return absolutize_path(target_dir);
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

pub(super) fn cargo_arg_value(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == flag {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
    }
    None
}

pub(super) fn absolutize_path(path: std::path::PathBuf) -> std::path::PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    }
}

pub(super) fn prepend_paths(
    dirs: &[std::path::PathBuf],
    existing_path: Option<&std::ffi::OsStr>,
) -> Result<std::ffi::OsString, SoldrError> {
    let mut paths: Vec<std::path::PathBuf> = dirs.to_vec();
    if let Some(existing_path) = existing_path {
        paths.extend(std::env::split_paths(existing_path));
    }
    std::env::join_paths(paths).map_err(|e| SoldrError::Other(format!("invalid PATH: {e}")))
}
