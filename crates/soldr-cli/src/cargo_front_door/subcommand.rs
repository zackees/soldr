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
//! - `cargo watch -x fmt` / `-s "cargo fmt"` introspection so the
//!   rustfmt zccache shim can be applied without engaging unrelated
//!   build-side hooks.
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
    first_cargo_subcommand_index(args).map(|idx| args[idx].as_str())
}

pub(crate) fn first_cargo_subcommand_index(args: &[String]) -> Option<usize> {
    let mut skip_next = false;
    for (idx, arg) in args.iter().enumerate() {
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
        return Some(idx);
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

/// Strip Cargo's rustup-proxy `+toolchain` directive from the leading global
/// arg region and return the selected toolchain without the `+`.
///
/// soldr resolves and execs a concrete cargo binary instead of rustup's cargo
/// proxy, so forwarding `+nightly` would make cargo treat it as a subcommand.
/// The front door maps the directive to `RUSTUP_TOOLCHAIN` instead.
pub(super) fn strip_cargo_toolchain_directive(args: &[String]) -> (Vec<String>, Option<String>) {
    let mut cleaned = Vec::with_capacity(args.len());
    let mut toolchain = None;
    let mut skip_next = false;
    let mut before_subcommand = true;

    for arg in args {
        if !before_subcommand {
            cleaned.push(arg.clone());
            continue;
        }
        if skip_next {
            skip_next = false;
            cleaned.push(arg.clone());
            continue;
        }
        if arg == "--" {
            before_subcommand = false;
            cleaned.push(arg.clone());
            continue;
        }
        if let Some(rest) = arg.strip_prefix('+').filter(|rest| !rest.is_empty()) {
            if toolchain.is_none() {
                toolchain = Some(rest.to_string());
                continue;
            }
        }
        if cargo_global_arg_takes_value(arg) {
            skip_next = !arg.contains('=');
            cleaned.push(arg.clone());
            continue;
        }
        if arg.starts_with('-') {
            cleaned.push(arg.clone());
            continue;
        }
        before_subcommand = false;
        cleaned.push(arg.clone());
    }

    (cleaned, toolchain)
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

/// Return whether an unmediated Cargo invocation may write compiler outputs.
///
/// This is deliberately broader than [`cargo_args_are_cacheable`]. That
/// predicate controls optional build-side hooks whose cost is inappropriate
/// for Miri and unknown plugins. The no-cache ownership preflight is a safety
/// boundary: a previous zccache hit may have left protected hardlinks in the
/// target directory, and any unwrapped compiler must detach them before
/// writing. Unknown cargo plugins therefore default to "may compile".
pub(crate) fn cargo_args_may_compile_unmediated(args: &[String]) -> bool {
    if cargo_args_are_cacheable(args) {
        return true;
    }

    let Some(subcommand) = first_cargo_subcommand(args) else {
        return false;
    };

    // These first-party commands compile without engaging Soldr's ordinary
    // build hooks: Miri owns its driver path, while package/publish perform a
    // verification build unless explicitly told not to. Conservatively
    // prepare the selected target directory for all three.
    if matches!(subcommand, "miri" | "package" | "publish") {
        return true;
    }

    // cargo-watch runs `check` by default and accepts arbitrary `-x` Cargo
    // verbs and `-s` shell commands. Even perfect argv parsing cannot prove an
    // arbitrary shell does not compile, so every watch invocation takes the
    // conservative ownership-preflight path.
    if subcommand == "watch" {
        return true;
    }

    // Every registered tool that wraps an inner build was accepted by
    // `cargo_args_are_cacheable` above. The remaining registered tools are
    // declared non-compiling in the registry and need no target traversal.
    if crate::fetch::lookup_by_cargo_subcommand(subcommand).is_some() {
        return false;
    }

    // Known first-party commands that do not compile. Keep this explicit so
    // an unfamiliar future Cargo verb, or an arbitrary cargo-* plugin, takes
    // the safe default below instead of silently bypassing the preflight.
    if matches!(
        subcommand,
        "add"
            | "clean"
            | "config"
            | "fetch"
            | "fmt"
            | "generate-lockfile"
            | "help"
            | "info"
            | "init"
            | "locate-project"
            | "login"
            | "logout"
            | "metadata"
            | "new"
            | "owner"
            | "pkgid"
            | "remove"
            | "report"
            | "search"
            | "tree"
            | "uninstall"
            | "update"
            | "vendor"
            | "verify-project"
            | "version"
            | "yank"
    ) {
        return false;
    }

    true
}

pub(crate) fn cargo_args_should_apply_rustfmt_shim(args: &[String]) -> bool {
    match first_cargo_subcommand(args) {
        Some("fmt") => true,
        Some("watch") => cargo_watch_inner_subcommands(args)
            .iter()
            .any(|sub| sub == "fmt"),
        _ => false,
    }
}

fn is_cacheable_cargo_subcommand(subcommand: &str) -> bool {
    // Naming note (issue #824 follow-up): this predicate no longer
    // controls the `RUSTC_WRAPPER=zccache` injection. The front door
    // now sets that env var on EVERY `soldr cargo ...` invocation when
    // caching is enabled, so zccache observes every rustc call (caching
    // the cacheable ones and passing through the rest). This predicate
    // exists to gate the OTHER per-build hooks that only make sense when
    // cargo is going to compile and touch `target/`:
    //
    //   - `cook_hydrate::maybe_hydrate` (cross-repo target/ pre-flight)
    //   - `profile_debug::maybe_apply_cargo_profile_debug_default`
    //   - `disk::maybe_emit_low_disk_warning`
    //   - `gc::disk::check_disk_or_warn_or_block` (host-volume watchdog)
    //   - target-registry memoization for the wrapper hot path (#440)
    //
    // None of those care about non-compiling cargo verbs like `metadata`
    // / `search` / `clean`, or about static-analysis cargo-* tools like
    // `deny` / `audit` / `machete`. So we still classify here. The name
    // is preserved for diff continuity with #824 even though "cacheable"
    // now means "engages the build-side hooks", not "controls
    // RUSTC_WRAPPER".

    // Cargo built-in verbs that compile, test, or document — intrinsic
    // build-side hook engagement.
    if matches!(
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
    ) {
        return true;
    }

    // `cargo miri` is intentionally not a build-side-hook verb. Miri
    // owns a distinct interpreter/runtime driver path, so Soldr does
    // not run target GC, rust-plan restore/save, or profile-debug
    // defaults around it. The cargo front door still injects
    // RUSTC_WRAPPER for every cache-enabled cargo invocation, so any
    // rustc child Miri launches can route through Soldr/zccache without
    // claiming the Miri driver itself is cacheable.

    // Managed cargo-<sub> ecosystem tool — consult the registry. The
    // `wraps_inner_cargo_build` flag is now the policy data: tools whose
    // outer invocation will spawn an inner cargo build (or otherwise
    // engage target/) should engage the build-side hooks too.
    //
    // `watch` is intentionally NOT covered here: `cargo watch -x build`'s
    // inner subcommand is parsed out of the argv by
    // `cargo_watch_inner_is_cacheable` in the cacheable-args composer
    // above, so the outer `watch` itself stays non-cacheable.
    crate::fetch::wraps_inner_cargo_build(subcommand)
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
    let idx = first_cargo_subcommand_index(args)?;
    (args[idx] == target).then_some(idx)
}

fn inner_subcommand_from_exec_value(value: &str) -> Option<String> {
    let mut tokens = value.split_whitespace();
    let mut first = tokens.next()?;
    // `-s 'cargo build --release'` form: peel off a literal leading `cargo`
    // so the next token is the inner subcommand.
    if first == "cargo" {
        first = tokens.next()?;
    }
    // Accept `cargo +nightly fmt` in shell forms. Leading cargo global
    // flags are intentionally not parsed here; this helper only handles
    // the common cargo-watch command strings we can classify safely.
    while first.starts_with('+') && first.len() > 1 {
        first = tokens.next()?;
    }
    Some(first.to_string())
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
