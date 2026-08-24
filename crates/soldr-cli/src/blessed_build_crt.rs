// Which C runtime the caller asked the MSVC cross-link to use (soldr#2794).
//
// `soldr build --target *-pc-windows-msvc` injects the UCRT/VCRuntime
// **import** libraries so clang-cl's default `/MD` mode resolves its
// `__declspec(dllimport)` references. That is right until the caller also
// passes `-C target-feature=+crt-static`, at which point rustc emits
// `/defaultlib:libcmt` and the link line carries *both* CRTs:
//
// ```text
// lld-link: error: duplicate symbol: __vcrt_InitializeCriticalSectionEx
//   >>> libvcruntime.lib(winapi_downlevel.obj)   (static)
//   >>> vcruntime.lib(VCRUNTIME140.dll)          (dynamic)
// ```
//
// soldr was hard-coding a *policy* (dynamic CRT) into what is otherwise
// toolchain *plumbing* (linker flavor + library search paths). This module
// reads the policy back off the caller instead. Upstream `cargo-xwin` made
// the same change in 0.20.0 (rust-cross/cargo-xwin#166); soldr needs it for
// `vcruntime` as well as `ucrt`, because soldr injects a `vcruntime` default
// that cargo-xwin does not.
//
// ## Everything here is pure
//
// Detection takes its inputs as arguments rather than reading the process
// environment, so the tests never mutate a shared variable. That is not
// incidental tidiness: `tests/env_lock_lint.rs` requires every mutated
// variable to sit under a single barrier, and `RUSTFLAGS` is already mutated
// under two others. A pure core sidesteps the question — only
// [`requested_crt_linkage`] touches the environment, and it does nothing but
// gather strings and delegate.

/// Which CRT the MSVC link line should pull in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrtLinkage {
    /// Import libraries — `ucrt.lib` / `vcruntime.lib`. soldr's default, and
    /// what clang-cl's `/MD` mode needs.
    Dynamic,
    /// Static archives — `libucrt.lib` / `libvcruntime.lib`. Selected when the
    /// caller has already said `+crt-static`.
    Static,
}

/// The `target-feature` spelling that selects each linkage.
const STATIC_FEATURE: &str = "+crt-static";
const DYNAMIC_FEATURE: &str = "-crt-static";

/// Decide the linkage from one flag string.
///
/// Returns `None` when the string says nothing about `crt-static`, which is
/// what lets [`crt_linkage_from_sources`] fall through to the next source
/// rather than treating silence as a decision.
///
/// Within a single string the **last** mention wins, matching how rustc
/// accumulates `-C target-feature` — `+crt-static ... -crt-static` really does
/// end up dynamic, and reporting it as static would produce exactly the
/// mismatched link line this module exists to prevent.
fn crt_linkage_in(flags: &str) -> Option<CrtLinkage> {
    let last_static = flags.rfind(STATIC_FEATURE);
    let last_dynamic = flags.rfind(DYNAMIC_FEATURE);
    match (last_static, last_dynamic) {
        (None, None) => None,
        (Some(_), None) => Some(CrtLinkage::Static),
        (None, Some(_)) => Some(CrtLinkage::Dynamic),
        (Some(s), Some(d)) => Some(if s > d {
            CrtLinkage::Static
        } else {
            CrtLinkage::Dynamic
        }),
    }
}

/// Decide the linkage from flag sources listed **highest precedence first**.
///
/// The first source that mentions `crt-static` decides; the rest are not
/// consulted. This mirrors Cargo, where the winning rustflags source replaces
/// the others outright rather than merging with them — so a `RUSTFLAGS` that
/// says nothing about the CRT does not veto a `.cargo/config.toml` that does.
pub(crate) fn crt_linkage_from_sources<I, S>(sources: I) -> CrtLinkage
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    sources
        .into_iter()
        .find_map(|source| crt_linkage_in(source.as_ref()))
        .unwrap_or(CrtLinkage::Dynamic)
}

/// `rustflags` entries from a `.cargo/config.toml` under `root`, most specific
/// first: `target.<triple>.rustflags` then `build.rustflags`.
///
/// Both string and array spellings are accepted. A missing, unreadable, or
/// malformed config yields nothing at all — this feeds a link-flag choice with
/// a working default, so it must never be the thing that fails a build.
pub(crate) fn cargo_config_rustflags(root: &std::path::Path, target_triple: &str) -> Vec<String> {
    let mut found = Vec::new();
    for relative in [".cargo/config.toml", ".cargo/config"] {
        let Ok(contents) = std::fs::read_to_string(root.join(relative)) else {
            continue;
        };
        let Ok(value) = contents.parse::<toml::Value>() else {
            continue;
        };
        let target_flags = value
            .get("target")
            .and_then(|targets| targets.get(target_triple))
            .and_then(|entry| entry.get("rustflags"))
            .and_then(flatten_rustflags);
        let build_flags = value
            .get("build")
            .and_then(|build| build.get("rustflags"))
            .and_then(flatten_rustflags);
        found.extend(target_flags);
        found.extend(build_flags);
        if !found.is_empty() {
            break;
        }
    }
    found
}

/// `"-C target-feature=+crt-static"` or `["-C", "target-feature=+crt-static"]`
/// alike, flattened to one whitespace-joined string.
fn flatten_rustflags(value: &toml::Value) -> Option<String> {
    let text = match value {
        toml::Value::String(value) => value.clone(),
        toml::Value::Array(values) => values
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

/// The linkage the caller has asked for, reading the environment and the
/// project's Cargo config in Cargo's own precedence order.
///
/// ## Why this is read here and not at `soldr prepare` time
///
/// `soldr prepare --github-env <FILE>` writes a *static snapshot*. In the
/// zccache action that motivated soldr#2794, `setup-soldr` runs some ninety
/// lines before the step that exports `+crt-static`, so a snapshot-time read
/// would look before the flag exists and silently never fire. This is called
/// while the flags are being constructed for the dispatch, which is the last
/// moment before they are used and the first at which the caller's own
/// `RUSTFLAGS` is guaranteed visible.
///
/// A caller who sets the flag *after* `soldr prepare` still gets the dynamic
/// default. That case is not fixable from here — nothing can read a variable
/// that does not exist yet — and it is why the Cargo-config sources are
/// consulted too: a `.cargo/config.toml` is on disk before either step runs.
pub(crate) fn requested_crt_linkage(target_triple: &str) -> CrtLinkage {
    let target_key = format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        target_triple.to_uppercase().replace('-', "_")
    );
    let mut sources = vec![
        // Cargo's precedence order for rustflags, highest first.
        std::env::var("CARGO_ENCODED_RUSTFLAGS")
            .unwrap_or_default()
            .replace('\u{1f}', " "),
        std::env::var(&target_key).unwrap_or_default(),
        std::env::var("RUSTFLAGS").unwrap_or_default(),
    ];
    if let Ok(current) = std::env::current_dir() {
        let root = project_root_for_crt(&current);
        sources.extend(cargo_config_rustflags(&root, target_triple));
    }
    crt_linkage_from_sources(sources)
}

/// Nearest ancestor holding a `Cargo.toml`, falling back to `start`.
fn project_root_for_crt(start: &std::path::Path) -> std::path::PathBuf {
    for directory in start.ancestors() {
        if directory.join("Cargo.toml").is_file() {
            return directory.to_path_buf();
        }
    }
    start.to_path_buf()
}
