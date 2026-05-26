//! Cargo subcommand sniffing and cacheability classification.
//!
//! This module owns:
//! - Locating the cargo subcommand inside an arg vector
//!   ([`first_cargo_subcommand`], [`cargo_subcommand_index`])
//! - Recognizing which cargo subcommands should engage `RUSTC_WRAPPER`
//!   ([`cargo_args_are_cacheable`], [`is_cacheable_cargo_subcommand`])
//! - `cargo watch -x <inner>` introspection so the inner subcommand
//!   classification flows through to the outer caller
//!   ([`cargo_watch_inner_is_cacheable`])
//! - `--target` / `--no-cache` argv probes
//!   ([`cargo_args_specify_target`], [`cargo_args_use_reserved_no_cache`])
//!
//! The helpers are all argv-only and intentionally do no I/O.

/// True when the cargo argv resolves to `cargo run` (or `cargo r`).
pub(super) fn is_cargo_run_invocation(args: &[String]) -> bool {
    matches!(first_cargo_subcommand(args), Some("run" | "r"))
}

/// Return the first positional argument (skipping flags) of the cargo
/// front-door args, which is conventionally the cargo subcommand.
pub(crate) fn first_cargo_subcommand(args: &[String]) -> Option<&str> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            break;
        }
        if arg.starts_with('+') && arg.len() > 1 {
            continue;
        }
        if cargo_global_arg_takes_value(arg) {
            skip_next = !arg.contains('=');
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

pub(super) fn cargo_global_arg_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-C" | "-Z"
            | "-j"
            | "--color"
            | "--config"
            | "--jobs"
            | "--manifest-path"
            | "--message-format"
            | "--target-dir"
    ) || arg.starts_with("-C=")
        || arg.starts_with("-Z=")
        || arg.starts_with("-j=")
        || arg.starts_with("--color=")
        || arg.starts_with("--config=")
        || arg.starts_with("--jobs=")
        || arg.starts_with("--manifest-path=")
        || arg.starts_with("--message-format=")
        || arg.starts_with("--target-dir=")
}

pub(crate) fn cargo_args_use_reserved_no_cache(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg == "--no-cache" {
            return true;
        }
    }
    false
}

pub(crate) fn cargo_args_are_cacheable(args: &[String]) -> bool {
    let Some(subcommand) = first_cargo_subcommand(args) else {
        return false;
    };

    if is_cacheable_cargo_subcommand(subcommand) {
        return true;
    }

    // cargo-watch (issue #341): the outer cargo invocation is `watch`, but the
    // process it spawns on every file change is `cargo <inner>`. If the inner
    // subcommand is itself cacheable, we want to seed `RUSTC_WRAPPER=zccache`
    // (and friends) on the cargo-watch process so the children inherit the
    // wrapper. Only `watch` is handled here; do not add other wrappers
    // (e.g. bacon) without verifying their argv shape.
    if subcommand == "watch" {
        return cargo_watch_inner_is_cacheable(args);
    }

    false
}

fn is_cacheable_cargo_subcommand(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "b" | "build"
            | "c"
            | "check"
            | "t"
            | "test"
            | "bench"
            | "d"
            | "doc"
            | "r"
            | "run"
            | "rustc"
            | "clippy"
            | "fix"
            | "install"
            | "nextest"
            // cargo-chef (powering `soldr cook`, issue #359): the outer
            // process orchestrates an inner `cargo build` against a stub
            // project. Treat `chef` as cacheable so RUSTC_WRAPPER, the
            // soldr-managed toolchain homes, and ZCCACHE_PATH_REMAP all
            // propagate to that inner build.
            | "chef"
    )
}

/// Scan a `cargo watch ...` arg list for `-x` / `--exec` / `-s` / `--shell`
/// values whose first whitespace-tokenized word names a cacheable cargo
/// subcommand. cargo-watch accepts multiple `-x` flags and runs them in
/// sequence; we treat the invocation as cacheable if ANY of them targets a
/// cacheable subcommand so the children get the wrapper. Shell form (`-s`)
/// may include a literal leading `cargo` token, which we strip before
/// classifying.
fn cargo_watch_inner_is_cacheable(args: &[String]) -> bool {
    cargo_watch_inner_subcommands(args)
        .iter()
        .any(|sub| is_cacheable_cargo_subcommand(sub))
}

fn cargo_watch_inner_subcommands(args: &[String]) -> Vec<String> {
    let mut subcommands = Vec::new();
    // Locate the outer `watch` subcommand using the same skip-flags-and-toolchain
    // pass `first_cargo_subcommand` uses, then walk only the args after it. This
    // keeps `-x` flags that happen to appear earlier (e.g. global flag values)
    // from being misread.
    let watch_idx = match cargo_subcommand_index(args, "watch") {
        Some(idx) => idx,
        None => return subcommands,
    };
    let mut iter = args.iter().skip(watch_idx + 1);
    while let Some(arg) = iter.next() {
        if arg == "--" {
            return subcommands;
        }

        // Long-form `--exec`/`--shell` with separate value.
        if arg == "--exec" || arg == "--shell" {
            if let Some(value) = iter.next() {
                if let Some(sub) = inner_subcommand_from_exec_value(value) {
                    subcommands.push(sub);
                }
            }
            continue;
        }
        // Long-form `--exec=...` / `--shell=...`.
        if let Some(value) = arg
            .strip_prefix("--exec=")
            .or_else(|| arg.strip_prefix("--shell="))
        {
            if let Some(sub) = inner_subcommand_from_exec_value(value) {
                subcommands.push(sub);
            }
            continue;
        }
        // Short-form `-x`/`-s` with separate value.
        if arg == "-x" || arg == "-s" {
            if let Some(value) = iter.next() {
                if let Some(sub) = inner_subcommand_from_exec_value(value) {
                    subcommands.push(sub);
                }
            }
            continue;
        }
        // Short-form `-x=...` / `-s=...`. cargo-watch typically uses
        // `-x VALUE`, but accept the `=` form defensively.
        if let Some(value) = arg.strip_prefix("-x=").or_else(|| arg.strip_prefix("-s=")) {
            if let Some(sub) = inner_subcommand_from_exec_value(value) {
                subcommands.push(sub);
            }
            continue;
        }
    }
    subcommands
}

/// Locate the index in `args` of `target`, skipping the same leading flags
/// (and `+toolchain` shorthand) that `first_cargo_subcommand` skips. Returns
/// `None` if no such positional appears before a `--` separator.
fn cargo_subcommand_index(args: &[String], target: &str) -> Option<usize> {
    let mut skip_next = false;
    for (idx, arg) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            return None;
        }
        if arg.starts_with('+') && arg.len() > 1 {
            continue;
        }
        if cargo_global_arg_takes_value(arg) {
            skip_next = !arg.contains('=');
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if arg == target {
            return Some(idx);
        }
        return None;
    }
    None
}

fn inner_subcommand_from_exec_value(value: &str) -> Option<String> {
    let mut tokens = value.split_whitespace();
    let first = tokens.next()?;
    // `-s 'cargo build --release'` form: peel off a literal leading `cargo`
    // so the next token is the inner subcommand.
    let candidate = if first == "cargo" {
        tokens.next()?
    } else {
        first
    };
    Some(candidate.to_string())
}

pub(crate) fn cargo_args_specify_target(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg == "--target" {
            return true;
        }
        if arg.starts_with("--target=") {
            return true;
        }
    }
    false
}

pub(super) fn cargo_args_target_value(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--target" {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix("--target=") {
            return Some(rest.to_string());
        }
    }
    None
}
