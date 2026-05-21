//! `soldr cargo run` trampoline (issue #344, slice 1 of #342).
//!
//! Intercepts `soldr cargo run` before the normal cargo front door spawns
//! cargo. If a sidecar at
//! `<target-dir>/<target?>/<profile>/.soldr-trampoline/<bin>.toml` proves
//! that the recorded source files + the binary haven't changed (mtime+size
//! oracle, same model cargo itself uses), the trampoline `exec`s the binary
//! directly. Any uncertainty falls through to real cargo. After a successful
//! cargo run on the fall-through path, the sidecar is refreshed by walking
//! the `.d` dep-info file cargo wrote.
//!
//! No content hashing — the bar is cargo's own correctness model.

use crate::resolve_toolchain_binary;
use serde::{Deserialize, Serialize};
use soldr_core::SoldrError;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

/// `--no-trampoline` opt-out flag (soldr-private). Stripped before
/// forwarding the arg list to cargo.
pub(crate) const NO_TRAMPOLINE_FLAG: &str = "--no-trampoline";

/// Env-var opt-out (`SOLDR_NO_TRAMPOLINE=1`).
pub(crate) const NO_TRAMPOLINE_ENV_VAR: &str = "SOLDR_NO_TRAMPOLINE";

/// Verbose tracing of fall-through reasons (`SOLDR_TRAMPOLINE_LOG=1`).
pub(crate) const TRAMPOLINE_LOG_ENV_VAR: &str = "SOLDR_TRAMPOLINE_LOG";

/// Outcome of `try_run_trampoline`.
pub(crate) enum TrampolineDecision {
    /// The trampoline executed the binary; soldr should exit with this code.
    Executed(i32),
    /// Fall through to real cargo. After cargo succeeds, the caller should
    /// invoke `refresh_sidecar_after_cargo` so the next invocation can hit
    /// the fast path.
    FellThrough(Box<FellThroughPlan>),
}

/// Information the caller needs to refresh the sidecar after a successful
/// cargo run. None of the fields are required for the fall-through itself —
/// they only feed the post-build sidecar refresh.
pub(crate) struct FellThroughPlan {
    pub(crate) parsed: Option<ParsedRunArgs>,
    /// The cleaned arg vector to forward to cargo (with `--no-trampoline`
    /// stripped).
    pub(crate) cleaned_args: Vec<String>,
    /// Bin path inferred from manifest+args; we'll stat this and its `.d`
    /// after cargo succeeds.
    pub(crate) binary_path: Option<PathBuf>,
    pub(crate) sidecar_path: Option<PathBuf>,
    pub(crate) dep_info_path: Option<PathBuf>,
}

/// Try the trampoline fast path. If conditions aren't right, returns
/// `FellThrough` with everything the caller needs to spawn cargo and then
/// refresh the sidecar.
pub(crate) fn try_run_trampoline(args: &[String]) -> Result<TrampolineDecision, SoldrError> {
    // First, strip `--no-trampoline` from the arg list regardless of what
    // happens; cargo doesn't understand it.
    let (cleaned_args, saw_no_trampoline_flag) = strip_no_trampoline_flag(args);

    // Build the fall-through plan up front. Even when opt-outs are set we
    // still want to refresh the sidecar after a successful build so the
    // *next* invocation (without the opt-out) hits the fast path.
    let parsed = parse_run_args(&cleaned_args);
    let plan = fell_through_plan(parsed.clone(), cleaned_args.clone());

    match try_fast_path(&cleaned_args, parsed, saw_no_trampoline_flag) {
        FastPathOutcome::Hit(binary, bin_args) => {
            log_hit(&binary);
            let code = exec_binary(&binary, &bin_args)?;
            Ok(TrampolineDecision::Executed(code))
        }
        FastPathOutcome::FallThrough(reason) => {
            log_fall_through(&reason);
            Ok(TrampolineDecision::FellThrough(Box::new(plan)))
        }
    }
}

enum FastPathOutcome {
    Hit(PathBuf, Vec<String>),
    FallThrough(String),
}

fn try_fast_path(
    cleaned_args: &[String],
    parsed: Option<ParsedRunArgs>,
    saw_no_trampoline_flag: bool,
) -> FastPathOutcome {
    if saw_no_trampoline_flag {
        return FastPathOutcome::FallThrough("opt-out: --no-trampoline flag".into());
    }
    if trampoline_env_disabled() {
        return FastPathOutcome::FallThrough(format!("opt-out: {NO_TRAMPOLINE_ENV_VAR} is set"));
    }
    let Some(parsed) = parsed else {
        return FastPathOutcome::FallThrough("cannot model these cargo args".into());
    };
    let bin = match resolve_bin_name(&parsed) {
        Ok(Some(name)) => name,
        Ok(None) => {
            return FastPathOutcome::FallThrough("could not determine unambiguous --bin".into())
        }
        Err(err) => return FastPathOutcome::FallThrough(format!("manifest read failed: {err}")),
    };
    let layout = compute_layout(&parsed, &bin);
    let binary = layout.binary_path;
    let sidecar = layout.sidecar_path;
    let sidecar_text = match fs::read_to_string(&sidecar) {
        Ok(text) => text,
        Err(err) => {
            return FastPathOutcome::FallThrough(format!(
                "sidecar missing: {} ({err})",
                sidecar.display()
            ))
        }
    };
    let sidecar_data: Sidecar = match toml::from_str(&sidecar_text) {
        Ok(data) => data,
        Err(err) => {
            return FastPathOutcome::FallThrough(format!(
                "sidecar parse failed: {} ({err})",
                sidecar.display()
            ))
        }
    };
    let bin_meta = match fs::metadata(&binary) {
        Ok(meta) => meta,
        Err(err) => {
            return FastPathOutcome::FallThrough(format!(
                "binary missing: {} ({err})",
                binary.display()
            ))
        }
    };
    let bin_mtime = mtime_nanos(&bin_meta);
    let bin_size = size_as_i64(&bin_meta);
    if bin_mtime != Some(sidecar_data.binary_mtime_nanos)
        || bin_size != Some(sidecar_data.binary_size_bytes)
    {
        return FastPathOutcome::FallThrough(format!(
            "binary {} mtime/size changed (sidecar mtime={}, size={}; on-disk mtime={:?}, size={:?})",
            binary.display(), sidecar_data.binary_mtime_nanos, sidecar_data.binary_size_bytes, bin_mtime, bin_size
        ));
    }
    let fingerprint = match compute_fingerprint(&parsed) {
        Ok(fp) => fp,
        Err(err) => {
            return FastPathOutcome::FallThrough(format!("fingerprint compute failed: {err}"))
        }
    };
    if fingerprint != sidecar_data.cargo_args_fingerprint {
        return FastPathOutcome::FallThrough(format!(
            "fingerprint mismatch (sidecar={}, current={})",
            sidecar_data.cargo_args_fingerprint, fingerprint
        ));
    }
    for entry in &sidecar_data.source_files {
        match fs::metadata(&entry.path) {
            Ok(meta) => {
                let mtime = mtime_nanos(&meta);
                let size = size_as_i64(&meta);
                if mtime != Some(entry.mtime_nanos) || size != Some(entry.size_bytes) {
                    return FastPathOutcome::FallThrough(format!(
                        "source {} mtime/size changed (recorded mtime={}, size={}; actual mtime={:?}, size={:?})",
                        entry.path, entry.mtime_nanos, entry.size_bytes, mtime, size
                    ));
                }
            }
            Err(err) => {
                return FastPathOutcome::FallThrough(format!(
                    "source missing: {} ({err})",
                    entry.path
                ));
            }
        }
    }
    FastPathOutcome::Hit(binary, trailing_user_args(cleaned_args))
}

/// After a successful `cargo run`, walk the dep-info file cargo wrote and
/// refresh the sidecar so the next invocation can hit the fast path.
pub(crate) fn refresh_sidecar_after_cargo(plan: &FellThroughPlan) {
    match build_and_write_sidecar(plan) {
        Ok(path) => log_event(&format!("wrote sidecar {}", path.display())),
        Err(reason) => log_fall_through(&format!("post-build: {reason}")),
    }
}

fn build_and_write_sidecar(plan: &FellThroughPlan) -> Result<PathBuf, String> {
    let parsed = plan.parsed.as_ref().ok_or("no parsed args")?;
    let binary = plan.binary_path.as_ref().ok_or("no binary path")?;
    let sidecar = plan.sidecar_path.as_ref().ok_or("no sidecar path")?;
    let dep_info = plan.dep_info_path.as_ref().ok_or("no dep-info path")?;

    let bin_meta = fs::metadata(binary)
        .map_err(|err| format!("binary stat failed: {} ({err})", binary.display()))?;
    let bin_mtime = mtime_nanos(&bin_meta).ok_or_else(|| "binary mtime unavailable".to_string())?;
    let bin_size = size_as_i64(&bin_meta).ok_or_else(|| "binary size unavailable".to_string())?;

    let dep_text = fs::read_to_string(dep_info)
        .map_err(|err| format!("dep-info missing: {} ({err})", dep_info.display()))?;
    let sources = parse_dep_info_for_output(&dep_text, binary)
        .ok_or_else(|| format!("dep-info has no stanza for {}", binary.display()))?;
    let fingerprint =
        compute_fingerprint(parsed).map_err(|err| format!("fingerprint compute failed: {err}"))?;

    let mut entries: Vec<SidecarSource> = Vec::with_capacity(sources.len());
    for src in sources {
        let meta = fs::metadata(&src)
            .map_err(|err| format!("source stat failed: {} ({err})", src.display()))?;
        let mtime = mtime_nanos(&meta)
            .ok_or_else(|| format!("source mtime unavailable for {}", src.display()))?;
        let size = size_as_i64(&meta)
            .ok_or_else(|| format!("source size unavailable for {}", src.display()))?;
        entries.push(SidecarSource {
            path: src.to_string_lossy().to_string(),
            mtime_nanos: mtime,
            size_bytes: size,
        });
    }

    let sidecar_data = Sidecar {
        binary_path: binary.to_string_lossy().to_string(),
        binary_mtime_nanos: bin_mtime,
        binary_size_bytes: bin_size,
        cargo_args_fingerprint: fingerprint,
        source_files: entries,
    };
    write_sidecar_atomic(sidecar, &sidecar_data)
        .map_err(|err| format!("sidecar write failed: {} ({err})", sidecar.display()))?;
    Ok(sidecar.clone())
}

// ---------------------------------------------------------------------------
// Arg parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedRunArgs {
    pub(crate) toolchain: Option<String>,
    pub(crate) bin: Option<String>,
    pub(crate) release: bool,
    pub(crate) profile: Option<String>,
    pub(crate) manifest_path: Option<PathBuf>,
    pub(crate) target: Option<String>,
    pub(crate) features: Vec<String>,
    pub(crate) all_features: bool,
    pub(crate) no_default_features: bool,
    pub(crate) target_dir: Option<PathBuf>,
    pub(crate) trailing: Vec<String>,
}

/// Strip `--no-trampoline` (soldr-private; not understood by cargo). Returns
/// the cleaned arg vec and whether the flag was present.
pub(crate) fn strip_no_trampoline_flag(args: &[String]) -> (Vec<String>, bool) {
    let mut out = Vec::with_capacity(args.len());
    let mut saw = false;
    let mut past_separator = false;
    for arg in args {
        if past_separator {
            out.push(arg.clone());
            continue;
        }
        if arg == "--" {
            past_separator = true;
            out.push(arg.clone());
            continue;
        }
        if arg == NO_TRAMPOLINE_FLAG {
            saw = true;
            continue;
        }
        out.push(arg.clone());
    }
    (out, saw)
}

/// Parse the slice of args after `cargo` (i.e. including the `run`
/// positional). Returns `None` if we encounter an arg we don't model (e.g.
/// `--example`, custom `--target-dir`, an unknown flag that takes a
/// value).
pub(crate) fn parse_run_args(args: &[String]) -> Option<ParsedRunArgs> {
    parse_cargo_args(args, &["run", "r"])
}

/// Shared parser for the trampoline. Accepts only invocations whose first
/// positional matches one of `accepted_subs`. The verb itself is consumed
/// but not recorded — callers already know which they asked for.
pub(crate) fn parse_cargo_args(args: &[String], accepted_subs: &[&str]) -> Option<ParsedRunArgs> {
    let mut toolchain: Option<String> = None;
    let mut bin: Option<String> = None;
    let mut release = false;
    let mut profile: Option<String> = None;
    let mut manifest_path: Option<PathBuf> = None;
    let mut target: Option<String> = None;
    let mut features: Vec<String> = Vec::new();
    let mut all_features = false;
    let mut no_default_features = false;
    let mut target_dir: Option<PathBuf> = None;

    let mut iter = args.iter();
    // Look for the verb, possibly after `+toolchain` and global flags.
    let mut saw_verb = false;
    while let Some(arg) = iter.next() {
        if arg == "--" {
            // Not yet at the verb — nothing here for us.
            return None;
        }
        if let Some(rest) = arg.strip_prefix('+') {
            if rest.is_empty() {
                return None;
            }
            toolchain = Some(rest.to_string());
            continue;
        }
        if global_flag_takes_value(arg) {
            // Skip the flag and its value (a few global flags like `--config`,
            // `--manifest-path` etc. can appear *before* the subcommand).
            // We don't model arbitrary global flags — bail.
            if arg == "--manifest-path" {
                manifest_path = iter.next().map(PathBuf::from);
                continue;
            }
            if let Some(value) = arg.strip_prefix("--manifest-path=") {
                manifest_path = Some(PathBuf::from(value));
                continue;
            }
            return None;
        }
        if arg.starts_with('-') {
            // A non-value-taking global flag we don't model.
            return None;
        }
        if accepted_subs.contains(&arg.as_str()) {
            saw_verb = true;
            break;
        }
        // Some other subcommand.
        return None;
    }
    if !saw_verb {
        return None;
    }

    while let Some(arg) = iter.next() {
        if arg == "--" {
            // The rest are the binary's argv. Capture them so we can replay.
            let trailing: Vec<String> = iter.cloned().collect();
            return Some(ParsedRunArgs {
                toolchain,
                bin,
                release,
                profile,
                manifest_path,
                target,
                features,
                all_features,
                no_default_features,
                target_dir,
                trailing,
            });
        }
        match arg.as_str() {
            "--bin" => {
                let value = iter.next()?;
                if bin.is_some() {
                    return None;
                }
                bin = Some(value.clone());
            }
            "--release" => release = true,
            "--profile" => {
                profile = Some(iter.next()?.clone());
            }
            "--manifest-path" => {
                manifest_path = Some(PathBuf::from(iter.next()?));
            }
            "--target" => {
                target = Some(iter.next()?.clone());
            }
            "--features" | "-F" => {
                features.extend(split_features(iter.next()?));
            }
            "--all-features" => all_features = true,
            "--no-default-features" => no_default_features = true,
            "--target-dir" => {
                target_dir = Some(PathBuf::from(iter.next()?));
            }
            "--example" => return None,
            "-q" | "--quiet" | "-v" | "--verbose" | "--locked" | "--frozen" | "--offline" => {}
            other => {
                if let Some(value) = other.strip_prefix("--bin=") {
                    if bin.is_some() {
                        return None;
                    }
                    bin = Some(value.to_string());
                } else if let Some(value) = other.strip_prefix("--profile=") {
                    profile = Some(value.to_string());
                } else if let Some(value) = other.strip_prefix("--manifest-path=") {
                    manifest_path = Some(PathBuf::from(value));
                } else if let Some(value) = other.strip_prefix("--target=") {
                    target = Some(value.to_string());
                } else if let Some(value) = other.strip_prefix("--features=") {
                    features.extend(split_features(value));
                } else if let Some(value) = other.strip_prefix("-F=") {
                    features.extend(split_features(value));
                } else if let Some(value) = other.strip_prefix("--target-dir=") {
                    target_dir = Some(PathBuf::from(value));
                } else if other.starts_with("--example") {
                    return None;
                } else if other.starts_with("-vv") || other.starts_with("--color") {
                    // Skip multi-v / `--color=...` — these don't affect the
                    // trampoline decision.
                } else {
                    return None;
                }
            }
        }
    }

    Some(ParsedRunArgs {
        toolchain,
        bin,
        release,
        profile,
        manifest_path,
        target,
        features,
        all_features,
        no_default_features,
        target_dir,
        trailing: Vec::new(),
    })
}

fn split_features(value: &str) -> Vec<String> {
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn global_flag_takes_value(arg: &str) -> bool {
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

fn trailing_user_args(args: &[String]) -> Vec<String> {
    let mut iter = args.iter().skip_while(|a| a.as_str() != "--");
    if iter.next().is_some() {
        iter.cloned().collect()
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Layout / binary / sidecar path resolution
// ---------------------------------------------------------------------------

pub(crate) struct Layout {
    pub(crate) binary_path: PathBuf,
    pub(crate) sidecar_path: PathBuf,
    pub(crate) dep_info_path: PathBuf,
}

pub(crate) fn compute_layout(parsed: &ParsedRunArgs, bin: &str) -> Layout {
    let manifest_path = parsed
        .manifest_path
        .clone()
        .unwrap_or_else(|| find_nearest_manifest().unwrap_or_else(|| PathBuf::from("Cargo.toml")));
    let manifest_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let target_dir = resolve_target_dir(parsed, &manifest_dir);

    let mut leaf = target_dir;
    if let Some(triple) = effective_target_triple(parsed) {
        leaf.push(triple);
    }
    let profile_dir = if parsed.release {
        "release".to_string()
    } else if let Some(profile) = parsed.profile.as_deref() {
        // cargo aliases `dev` to `debug` on disk.
        if profile == "dev" {
            "debug".to_string()
        } else {
            profile.to_string()
        }
    } else {
        "debug".to_string()
    };
    leaf.push(&profile_dir);

    let bin_filename = if cfg!(windows) {
        format!("{bin}.exe")
    } else {
        bin.to_string()
    };
    let binary_path = leaf.join(&bin_filename);
    let sidecar_path = leaf.join(".soldr-trampoline").join(format!("{bin}.toml"));
    let dep_info_path = leaf.join(format!("{bin}.d"));

    Layout {
        binary_path,
        sidecar_path,
        dep_info_path,
    }
}

pub(crate) fn resolve_target_dir(parsed: &ParsedRunArgs, manifest_dir: &Path) -> PathBuf {
    if let Some(explicit) = parsed.target_dir.as_ref() {
        return absolutize(explicit.clone(), manifest_dir);
    }
    if let Some(env_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        if !env_dir.is_empty() {
            return absolutize(PathBuf::from(env_dir), manifest_dir);
        }
    }
    manifest_dir.join("target")
}

/// Determine the target subdirectory cargo will produce artifacts under.
///
/// Precedence mirrors `cargo_front_door::default_cargo_build_target`:
/// explicit `--target`, then `CARGO_BUILD_TARGET` env var, and on Windows
/// the auto-detected host triple that soldr injects when neither of those
/// is set. On non-Windows hosts cargo defaults to the unprefixed
/// `target/<profile>/` layout, so we return `None`.
pub(crate) fn effective_target_triple(parsed: &ParsedRunArgs) -> Option<String> {
    if let Some(t) = parsed.target.as_deref() {
        return Some(t.to_string());
    }
    if let Some(v) = std::env::var_os("CARGO_BUILD_TARGET") {
        if let Some(s) = v.to_str() {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    if cfg!(windows) {
        soldr_core::TargetTriple::detect().ok().map(|t| t.triple())
    } else {
        None
    }
}

fn absolutize(path: PathBuf, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

pub(crate) fn find_nearest_manifest() -> Option<PathBuf> {
    let mut current = std::env::current_dir().ok()?;
    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Resolve the `[bin]` name to build. Slice 1 only handles unambiguous
/// cases: explicit `--bin <NAME>`, or a crate with exactly one binary.
/// Returns `Ok(None)` when ambiguity forces fall-through.
pub(crate) fn resolve_bin_name(parsed: &ParsedRunArgs) -> Result<Option<String>, SoldrError> {
    if let Some(name) = parsed.bin.as_ref() {
        return Ok(Some(name.clone()));
    }
    let manifest_path = parsed
        .manifest_path
        .clone()
        .or_else(find_nearest_manifest)
        .ok_or_else(|| SoldrError::Other("Cargo.toml not found".into()))?;
    let text = fs::read_to_string(&manifest_path).map_err(|err| {
        SoldrError::Other(format!("read manifest {}: {err}", manifest_path.display()))
    })?;
    let value: toml::Value = text.parse().map_err(|err| {
        SoldrError::Other(format!("parse manifest {}: {err}", manifest_path.display()))
    })?;

    let bins = value.get("bin").and_then(|v| v.as_array());
    let package_name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string);

    let manifest_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    // Case 1: explicit `[[bin]]` table(s).
    if let Some(bins) = bins {
        if bins.len() == 1 {
            let entry = &bins[0];
            if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                return Ok(Some(name.to_string()));
            }
        }
        if !bins.is_empty() {
            return Ok(None);
        }
    }

    // Case 2: no `[[bin]]` table, but `src/main.rs` exists → cargo treats it
    // as a default binary named after the package.
    let auto_main = manifest_dir.join("src").join("main.rs");
    let auto_bin_dir = manifest_dir.join("src").join("bin");
    let auto_bins: Vec<PathBuf> = if auto_bin_dir.is_dir() {
        match fs::read_dir(&auto_bin_dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("rs"))
                .collect(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let mut candidates: Vec<String> = Vec::new();
    if auto_main.is_file() {
        if let Some(name) = package_name.as_deref() {
            candidates.push(name.to_string());
        }
    }
    for entry in &auto_bins {
        if let Some(stem) = entry.file_stem().and_then(|s| s.to_str()) {
            candidates.push(stem.to_string());
        }
    }

    if candidates.len() == 1 {
        return Ok(Some(candidates.remove(0)));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Sidecar (de)serialization
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Sidecar {
    pub(crate) binary_path: String,
    pub(crate) binary_mtime_nanos: i64,
    pub(crate) binary_size_bytes: i64,
    pub(crate) cargo_args_fingerprint: String,
    #[serde(default, rename = "source_files")]
    pub(crate) source_files: Vec<SidecarSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SidecarSource {
    pub(crate) path: String,
    pub(crate) mtime_nanos: i64,
    pub(crate) size_bytes: i64,
}

pub(crate) fn write_sidecar_atomic(path: &Path, data: &Sidecar) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string(data)
        .map_err(|err| std::io::Error::other(format!("toml serialize: {err}")))?;
    let mut tmp_name: OsString = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all().ok();
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Fingerprint
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct FingerprintInputs<'a> {
    profile: &'a str,
    target: Option<&'a str>,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    manifest_path: String,
    rustflags: String,
    rustc_identity: String,
    /// Digest of every `.cargo/config.toml` (+ legacy `.cargo/config`)
    /// cargo would discover walking from the manifest dir up to the
    /// filesystem root, plus `$CARGO_HOME/config.toml`. Added for #346:
    /// the trampoline previously only hashed `RUSTFLAGS` from the env,
    /// so a config-file edit silently fast-pathed the stale binary.
    cargo_config_digest: String,
}

pub(crate) fn compute_fingerprint(parsed: &ParsedRunArgs) -> Result<String, SoldrError> {
    let profile = if parsed.release {
        "release"
    } else if let Some(p) = parsed.profile.as_deref() {
        if p == "dev" {
            "debug"
        } else {
            p
        }
    } else {
        "debug"
    };

    let manifest_path = parsed
        .manifest_path
        .clone()
        .or_else(find_nearest_manifest)
        .ok_or_else(|| SoldrError::Other("Cargo.toml not found for fingerprint".into()))?;
    let canonical = fs::canonicalize(&manifest_path).unwrap_or(manifest_path);
    let manifest_dir = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let manifest_string = canonical.to_string_lossy().to_string();

    let mut features: Vec<String> = parsed.features.clone();
    features.sort();
    features.dedup();

    let rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
    let rustc_identity = rustc_identity_cached()?;
    let cargo_config_digest = cargo_config_digest(&manifest_dir);

    let effective_target = effective_target_triple(parsed);
    let inputs = FingerprintInputs {
        profile,
        target: effective_target.as_deref(),
        features,
        all_features: parsed.all_features,
        no_default_features: parsed.no_default_features,
        manifest_path: manifest_string,
        rustflags,
        rustc_identity,
        cargo_config_digest,
    };

    let bytes = serde_json::to_vec(&inputs)
        .map_err(|err| SoldrError::Other(format!("fingerprint serialize: {err}")))?;
    let hash = blake3::hash(&bytes);
    Ok(format!("blake3:{}", hash.to_hex()))
}

static RUSTC_IDENTITY_CACHE: OnceLock<String> = OnceLock::new();

fn rustc_identity_cached() -> Result<String, SoldrError> {
    if let Some(cached) = RUSTC_IDENTITY_CACHE.get() {
        return Ok(cached.clone());
    }
    let rustc = resolve_toolchain_binary("rustc")?;
    let output = std::process::Command::new(&rustc)
        .arg("-vV")
        .output()
        .map_err(|err| SoldrError::Other(format!("rustc -vV: {err}")))?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "rustc -vV exited with {}",
            output.status
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let _ = RUSTC_IDENTITY_CACHE.set(text.clone());
    Ok(text)
}

// ---------------------------------------------------------------------------
// Dep-info parsing — see [`dep_info`] sibling module.
// ---------------------------------------------------------------------------

#[path = "trampoline_dep_info.rs"]
mod dep_info;
pub(crate) use dep_info::parse_dep_info_for_output;

// ---------------------------------------------------------------------------
// `.cargo/config.toml` content digest (issue #346) — see [`config`] sibling
// module.
// ---------------------------------------------------------------------------

#[path = "trampoline_config.rs"]
mod config;
pub(crate) use config::cargo_config_digest;

// ---------------------------------------------------------------------------
// Exec / stat / logging helpers
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn exec_binary(binary: &Path, args: &[String]) -> Result<i32, SoldrError> {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(binary).args(args).exec();
    Err(SoldrError::Other(format!(
        "failed to exec {}: {err}",
        binary.display()
    )))
}

#[cfg(not(unix))]
fn exec_binary(binary: &Path, args: &[String]) -> Result<i32, SoldrError> {
    let status = std::process::Command::new(binary)
        .args(args)
        .status()
        .map_err(|err| SoldrError::Other(format!("failed to spawn {}: {err}", binary.display())))?;
    Ok(status.code().unwrap_or(1))
}

pub(crate) fn mtime_nanos(meta: &fs::Metadata) -> Option<i64> {
    let mtime = meta.modified().ok()?;
    let dur = mtime.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    i64::try_from(dur.as_nanos()).ok()
}

pub(crate) fn size_as_i64(meta: &fs::Metadata) -> Option<i64> {
    i64::try_from(meta.len()).ok()
}

pub(crate) fn trampoline_env_disabled() -> bool {
    matches!(
        std::env::var(NO_TRAMPOLINE_ENV_VAR).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

pub(crate) fn trampoline_log_enabled() -> bool {
    matches!(
        std::env::var(TRAMPOLINE_LOG_ENV_VAR).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

pub(crate) fn log_fall_through(reason: &str) {
    if trampoline_log_enabled() {
        eprintln!("soldr trampoline: fall-through: {reason}");
    }
}

pub(crate) fn log_event(message: &str) {
    if trampoline_log_enabled() {
        eprintln!("soldr trampoline: {message}");
    }
}

fn log_hit(binary: &Path) {
    if trampoline_log_enabled() {
        eprintln!("soldr trampoline: hit, exec {}", binary.display());
    }
}

fn fell_through_plan(parsed: Option<ParsedRunArgs>, cleaned_args: Vec<String>) -> FellThroughPlan {
    let (binary_path, sidecar_path, dep_info_path) = match parsed.as_ref() {
        Some(p) => match resolve_bin_name(p) {
            Ok(Some(name)) => {
                let layout = compute_layout(p, &name);
                (
                    Some(layout.binary_path),
                    Some(layout.sidecar_path),
                    Some(layout.dep_info_path),
                )
            }
            _ => (None, None, None),
        },
        None => (None, None, None),
    };
    FellThroughPlan {
        parsed,
        cleaned_args,
        binary_path,
        sidecar_path,
        dep_info_path,
    }
}

#[cfg(test)]
#[path = "trampoline_tests.rs"]
mod tests;
