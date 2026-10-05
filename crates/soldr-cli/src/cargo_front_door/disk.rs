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

/// Colorize stderr only when it is a terminal and `NO_COLOR` is unset. A
/// redirected or captured stderr (CI logs, `2>file`) always gets plain text.
pub(crate) fn stderr_should_use_color() -> bool {
    use std::io::IsTerminal;

    color_enabled(
        std::env::var_os("NO_COLOR").is_some(),
        std::io::stderr().is_terminal(),
    )
}

/// The pure rule behind [`stderr_should_use_color`]: any `NO_COLOR` value
/// disables color (no-color.org), and so does a non-terminal sink.
pub(crate) fn color_enabled(no_color_set: bool, stderr_is_terminal: bool) -> bool {
    !no_color_set && stderr_is_terminal
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
    crate::platform::fs::volume::free_bytes(path)
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

/// Assemble the child cargo's PATH: `dirs` first (in declaration order),
/// then the inherited PATH, with duplicates removed from both halves.
///
/// soldr#3485: `dirs` carries the fetched `cargo-<sub>` tool's directory,
/// which is version-scoped (`bin/cargo-chef-0.1.73`), and it used to be
/// prepended unconditionally. That made the child's PATH hash differ between
/// phases of one workflow — `soldr cook` carried the chef dir while the
/// following `soldr cargo test` did not — so every PATH-tracking build
/// script (pyo3's `rerun-if-env-changed=PATH` in particular) saw a dirty
/// env between phases and rebuilt. Nested front-door invocations also
/// re-prepended dirs the parent had already added, growing a duplicate per
/// nesting level.
///
/// The rule now: a dir already present in the inherited PATH keeps its
/// position and is not re-added — so re-assembling over a PATH that already
/// contains every dir is byte-identical to the input, and a tool dir that
/// is already resolvable never moves. A dir repeated within `dirs` is
/// added once. Missing dirs keep the declaration order they are given.
/// Note that a dir already resolvable *earlier* on the inherited PATH wins
/// over a later fetched copy — cargo's own dispatch would have deferred to
/// that binary anyway (`path_deferred_subcommand_tool`), so soldr only
/// ever fills gaps.
pub(super) fn prepend_paths(
    dirs: &[std::path::PathBuf],
    existing_path: Option<&std::ffi::OsStr>,
) -> Result<std::ffi::OsString, SoldrError> {
    let existing: Vec<std::path::PathBuf> = existing_path
        .map(|value| std::env::split_paths(value).collect())
        .unwrap_or_default();
    let mut paths: Vec<std::path::PathBuf> = Vec::with_capacity(dirs.len() + existing.len());
    for dir in dirs {
        if existing.iter().any(|entry| entry == dir) || paths.iter().any(|entry| entry == dir) {
            continue;
        }
        paths.push(dir.clone());
    }
    paths.extend(existing);
    std::env::join_paths(paths).map_err(|e| SoldrError::Other(format!("invalid PATH: {e}")))
}

#[cfg(test)]
mod tests {
    use super::prepend_paths;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn join(entries: &[&str]) -> OsString {
        std::env::join_paths(entries.iter().map(PathBuf::from)).expect("join paths")
    }

    fn dirs(entries: &[&str]) -> Vec<PathBuf> {
        entries.iter().map(PathBuf::from).collect()
    }

    /// soldr#3485: a missing tool dir is still prepended, ahead of the
    /// inherited PATH, in the order the caller declared it.
    #[test]
    fn missing_dirs_are_prepended_in_declaration_order() {
        let existing = join(&["/usr/bin", "/bin"]);
        let assembled =
            prepend_paths(&dirs(&["/tools/a", "/tools/b"]), Some(&existing)).expect("prepend");
        assert_eq!(
            assembled,
            join(&["/tools/a", "/tools/b", "/usr/bin", "/bin"])
        );
    }

    /// The headline observable of soldr#3485: when the tool dir is already
    /// resolvable, insertion must not change PATH at all — byte for byte —
    /// so a PATH-tracking build script's `rerun-if-env-changed=PATH` never
    /// fires between two invocations that resolve the same tool.
    #[test]
    fn a_tool_dir_already_present_leaves_path_byte_identical() {
        let existing = join(&["/usr/bin", "/tools/cargo-chef-0.1.73", "/bin"]);
        let assembled =
            prepend_paths(&dirs(&["/tools/cargo-chef-0.1.73"]), Some(&existing)).expect("prepend");
        assert_eq!(
            assembled, existing,
            "an already-present tool dir must not be re-added or moved"
        );
    }

    /// A nested front-door invocation inherits the PATH the parent front door
    /// already assembled. Re-running assembly over it must be a no-op —
    /// otherwise every nesting level added another copy of the same dir.
    #[test]
    fn reassembly_over_an_already_assembled_path_adds_nothing() {
        let first = prepend_paths(&dirs(&["/tools/cargo-nextest-0.9"]), None).expect("first");
        let second =
            prepend_paths(&dirs(&["/tools/cargo-nextest-0.9"]), Some(&first)).expect("second");
        assert_eq!(second, first);
        let third =
            prepend_paths(&dirs(&["/tools/cargo-nextest-0.9"]), Some(&second)).expect("third");
        assert_eq!(third, first);
    }

    /// Duplicates within one batch collapse to a single entry, still ahead
    /// of the inherited PATH and still in first-seen order.
    #[test]
    fn duplicate_dirs_within_the_batch_are_added_once() {
        let existing = join(&["/usr/bin"]);
        let assembled = prepend_paths(
            &dirs(&["/tools/a", "/tools/b", "/tools/a", "/tools/b"]),
            Some(&existing),
        )
        .expect("prepend");
        assert_eq!(assembled, join(&["/tools/a", "/tools/b", "/usr/bin"]));
    }

    /// Two invocations with the same inputs assemble byte-identical PATHs
    /// (the deterministic half of soldr#3485's acceptance).
    #[test]
    fn assembly_is_byte_stable_for_repeated_invocations() {
        let existing = join(&["/usr/bin", "/bin"]);
        let batch = dirs(&["/shims", "/tools/cargo-chef-0.1.73"]);
        let first = prepend_paths(&batch, Some(&existing)).expect("first");
        let second = prepend_paths(&batch, Some(&existing)).expect("second");
        assert_eq!(first, second);
    }

    /// No inherited PATH: the batch stands alone, still deduplicated.
    #[test]
    fn without_an_inherited_path_the_batch_is_the_result() {
        let assembled = prepend_paths(&dirs(&["/tools/a", "/tools/a"]), None).expect("prepend");
        assert_eq!(assembled, join(&["/tools/a"]));
    }
}
