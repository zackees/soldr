//! Workspace-level trampoline for `soldr cargo build`, `soldr cargo
//! check`, and `soldr cargo clippy` (issue #354, Tier L3 of #352).
//!
//! Extends the per-binary `cargo run` trampoline ([`crate::trampoline`]) to
//! the no-run build-side verbs. Unlike `cargo run` — which has exactly one
//! binary to exec — workspace builds can produce multiple outputs (one bin,
//! N rlibs, etc.) and `check` produces only `.rmeta` files. `clippy` has no
//! on-disk artifact at all; the "output" is its diagnostic stream.
//!
//! The freshness oracle is the same as cargo's own (mtime + size). The
//! sidecar lives at
//! `<target-dir>/<target?>/<profile>/.soldr-trampoline/workspace-<verb>.toml`.
//!
//! Decision is **all-or-nothing**. If any recorded output is missing or any
//! recorded source's stat doesn't match, we fall through to real cargo —
//! partial skip would leave cargo's incremental state inconsistent.

use crate::core::SoldrError;
use crate::trampoline::{
    compute_fingerprint, effective_target_triple, find_nearest_manifest, log_event,
    log_fall_through, mtime_nanos, parse_cargo_args, resolve_target_dir, size_as_i64,
    strip_no_trampoline_flag, trampoline_env_disabled, ParsedRunArgs, SidecarSource,
    NO_TRAMPOLINE_ENV_VAR,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Which workspace-mode verb the trampoline is operating on. Drives sidecar
/// filename, on-disk artifact discovery, and the skip-action behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceVerb {
    Build,
    Check,
    Clippy,
}

impl WorkspaceVerb {
    pub(crate) fn label(self) -> &'static str {
        match self {
            WorkspaceVerb::Build => "build",
            WorkspaceVerb::Check => "check",
            WorkspaceVerb::Clippy => "clippy",
        }
    }

    fn accepted_subs(self) -> &'static [&'static str] {
        match self {
            WorkspaceVerb::Build => &["build", "b"],
            WorkspaceVerb::Check => &["check", "c"],
            WorkspaceVerb::Clippy => &["clippy"],
        }
    }

    fn sidecar_filename(self) -> String {
        format!("workspace-{}.toml", self.label())
    }
}

/// Detect whether `args` is a workspace-mode invocation we know how to
/// trampoline. Returns the matching verb if so.
pub(crate) fn detect_workspace_verb(args: &[String]) -> Option<WorkspaceVerb> {
    let sub = crate::cargo_front_door::first_cargo_subcommand(args)?;
    match sub {
        "build" | "b" => Some(WorkspaceVerb::Build),
        "check" | "c" => Some(WorkspaceVerb::Check),
        "clippy" => Some(WorkspaceVerb::Clippy),
        _ => None,
    }
}

/// Plan returned on fall-through. The caller (cargo front door) hands this
/// back to [`refresh_workspace_sidecar_after_cargo`] after cargo succeeds.
pub(crate) struct WorkspaceFellThroughPlan {
    pub(crate) verb: WorkspaceVerb,
    pub(crate) parsed: Option<ParsedRunArgs>,
    pub(crate) cleaned_args: Vec<String>,
    pub(crate) profile_dir: Option<PathBuf>,
    pub(crate) sidecar_path: Option<PathBuf>,
}

pub(crate) enum WorkspaceDecision {
    /// Sidecar proved freshness; the trampoline already performed the
    /// skip action (printed nothing or replayed cached clippy output).
    Skipped(i32),
    /// Fall through to real cargo. After success, call
    /// [`refresh_workspace_sidecar_after_cargo`].
    FellThrough(Box<WorkspaceFellThroughPlan>),
}

/// Attempt the fast path for a workspace-mode verb. Mirrors
/// [`crate::trampoline::try_run_trampoline`] but does not exec a binary.
pub(crate) fn try_workspace_trampoline(
    verb: WorkspaceVerb,
    args: &[String],
) -> Result<WorkspaceDecision, SoldrError> {
    let (cleaned_args, saw_no_trampoline_flag) = strip_no_trampoline_flag(args);
    let parsed = parse_cargo_args(&cleaned_args, verb.accepted_subs());
    let plan = workspace_fell_through_plan(verb, parsed.clone(), cleaned_args.clone());

    match try_fast_path(verb, parsed, saw_no_trampoline_flag) {
        WsFastPathOutcome::Hit { exit_code } => Ok(WorkspaceDecision::Skipped(exit_code)),
        WsFastPathOutcome::FallThrough(reason) => {
            log_fall_through(&reason);
            Ok(WorkspaceDecision::FellThrough(Box::new(plan)))
        }
    }
}

enum WsFastPathOutcome {
    Hit { exit_code: i32 },
    FallThrough(String),
}

fn try_fast_path(
    verb: WorkspaceVerb,
    parsed: Option<ParsedRunArgs>,
    saw_no_trampoline_flag: bool,
) -> WsFastPathOutcome {
    if saw_no_trampoline_flag {
        return WsFastPathOutcome::FallThrough("opt-out: --no-trampoline flag".into());
    }
    if trampoline_env_disabled() {
        return WsFastPathOutcome::FallThrough(format!("opt-out: {NO_TRAMPOLINE_ENV_VAR} is set"));
    }
    let Some(parsed) = parsed else {
        return WsFastPathOutcome::FallThrough("cannot model these cargo args".into());
    };
    let profile_dir = compute_profile_dir(&parsed);
    let sidecar_path = profile_dir
        .join(".soldr-trampoline")
        .join(verb.sidecar_filename());

    let sidecar_text = match fs::read_to_string(&sidecar_path) {
        Ok(text) => text,
        Err(err) => {
            return WsFastPathOutcome::FallThrough(format!(
                "sidecar missing: {} ({err})",
                sidecar_path.display()
            ));
        }
    };
    let sidecar: WorkspaceSidecar = match toml::from_str(&sidecar_text) {
        Ok(data) => data,
        Err(err) => {
            return WsFastPathOutcome::FallThrough(format!(
                "sidecar parse failed: {} ({err})",
                sidecar_path.display()
            ));
        }
    };
    if sidecar.schema_version != WORKSPACE_SIDECAR_SCHEMA_VERSION {
        return WsFastPathOutcome::FallThrough(format!(
            "sidecar schema {} != expected {}",
            sidecar.schema_version, WORKSPACE_SIDECAR_SCHEMA_VERSION
        ));
    }
    if sidecar.verb != verb.label() {
        return WsFastPathOutcome::FallThrough(format!(
            "sidecar verb mismatch (recorded={}, current={})",
            sidecar.verb,
            verb.label()
        ));
    }

    let fingerprint = match compute_fingerprint(&parsed) {
        Ok(fp) => fp,
        Err(err) => {
            return WsFastPathOutcome::FallThrough(format!("fingerprint compute failed: {err}"));
        }
    };
    if fingerprint != sidecar.cargo_args_fingerprint {
        return WsFastPathOutcome::FallThrough(format!(
            "fingerprint mismatch (sidecar={}, current={})",
            sidecar.cargo_args_fingerprint, fingerprint
        ));
    }

    // All outputs must be present on disk AND match recorded mtime+size.
    // For `clippy` the artifact list may be empty (no on-disk output) — we
    // still iterate to keep the check uniform.
    for output in &sidecar.outputs {
        match fs::metadata(&output.path) {
            Ok(meta) => {
                let mtime = mtime_nanos(&meta);
                let size = size_as_i64(&meta);
                if mtime != Some(output.mtime_nanos) || size != Some(output.size_bytes) {
                    return WsFastPathOutcome::FallThrough(format!(
                        "output {} mtime/size changed (recorded mtime={}, size={}; actual mtime={:?}, size={:?})",
                        output.path, output.mtime_nanos, output.size_bytes, mtime, size
                    ));
                }
            }
            Err(err) => {
                return WsFastPathOutcome::FallThrough(format!(
                    "output missing: {} ({err})",
                    output.path
                ));
            }
        }
    }

    // Sources may legitimately be empty if cargo wrote no `.d` files (e.g.
    // a no-op build with everything cached). Treat empty as "no
    // freshness signal" → fall through, so the cold-build refresh runs.
    if sidecar.source_files.is_empty() {
        return WsFastPathOutcome::FallThrough("no source files recorded in sidecar".into());
    }
    for entry in &sidecar.source_files {
        match fs::metadata(&entry.path) {
            Ok(meta) => {
                let mtime = mtime_nanos(&meta);
                let size = size_as_i64(&meta);
                if mtime != Some(entry.mtime_nanos) || size != Some(entry.size_bytes) {
                    return WsFastPathOutcome::FallThrough(format!(
                        "source {} mtime/size changed (recorded mtime={}, size={}; actual mtime={:?}, size={:?})",
                        entry.path, entry.mtime_nanos, entry.size_bytes, mtime, size
                    ));
                }
            }
            Err(err) => {
                return WsFastPathOutcome::FallThrough(format!(
                    "source missing: {} ({err})",
                    entry.path
                ));
            }
        }
    }

    // Sidecar is fresh. Perform the skip action.
    match verb {
        WorkspaceVerb::Build | WorkspaceVerb::Check => {
            log_event(&format!("skipping cargo {} (cached)", verb.label()));
            WsFastPathOutcome::Hit { exit_code: 0 }
        }
        WorkspaceVerb::Clippy => match replay_clippy_capture(&sidecar) {
            Ok(code) => {
                log_event("replayed clippy diagnostics from sidecar");
                WsFastPathOutcome::Hit { exit_code: code }
            }
            Err(reason) => {
                WsFastPathOutcome::FallThrough(format!("clippy replay failed: {reason}"))
            }
        },
    }
}

fn replay_clippy_capture(sidecar: &WorkspaceSidecar) -> Result<i32, String> {
    let capture = sidecar
        .clippy_capture
        .as_ref()
        .ok_or_else(|| "no clippy_capture in sidecar".to_string())?;
    let stdout_bytes = read_gzip_file(Path::new(&capture.stdout_path))
        .map_err(|err| format!("read {}: {err}", capture.stdout_path))?;
    let stderr_bytes = read_gzip_file(Path::new(&capture.stderr_path))
        .map_err(|err| format!("read {}: {err}", capture.stderr_path))?;
    {
        let stdout = std::io::stdout();
        let mut h = stdout.lock();
        h.write_all(&stdout_bytes)
            .map_err(|err| format!("write stdout: {err}"))?;
        let _ = h.flush();
    }
    {
        let stderr = std::io::stderr();
        let mut h = stderr.lock();
        h.write_all(&stderr_bytes)
            .map_err(|err| format!("write stderr: {err}"))?;
        let _ = h.flush();
    }
    Ok(capture.exit_code)
}

/// Refresh the workspace sidecar after a successful cargo run. Walks the
/// `target/<profile>/` (and `deps/`) directories for `.d` files cargo
/// wrote, unions their sources, and stats every recorded output artifact.
///
/// `clippy_capture` (when present) is the captured stdout/stderr/exit-code
/// from the clippy run. Callers pass `None` for non-clippy verbs.
pub(crate) fn refresh_workspace_sidecar_after_cargo(
    plan: &WorkspaceFellThroughPlan,
    clippy_capture: Option<RawClippyCapture>,
) {
    match build_and_write_workspace_sidecar(plan, clippy_capture) {
        Ok(path) => log_event(&format!("wrote sidecar {}", path.display())),
        Err(reason) => log_fall_through(&format!("post-build: {reason}")),
    }
}

/// Raw capture of a clippy run's stdout/stderr/exit-code; the trampoline
/// will gzip the bytes into sibling files alongside the sidecar.
pub(crate) struct RawClippyCapture {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) exit_code: i32,
}

fn build_and_write_workspace_sidecar(
    plan: &WorkspaceFellThroughPlan,
    clippy_capture: Option<RawClippyCapture>,
) -> Result<PathBuf, String> {
    let parsed = plan.parsed.as_ref().ok_or("no parsed args")?;
    let profile_dir = plan.profile_dir.as_ref().ok_or("no profile dir")?;
    let sidecar_path = plan.sidecar_path.as_ref().ok_or("no sidecar path")?;

    let fingerprint =
        compute_fingerprint(parsed).map_err(|err| format!("fingerprint compute failed: {err}"))?;

    // Discover every `.d` file cargo wrote under <profile>/ and <profile>/deps/.
    let dep_files = enumerate_dep_files(profile_dir);

    let mut outputs: Vec<WorkspaceOutput> = Vec::new();
    let mut sources: BTreeSet<PathBuf> = BTreeSet::new();
    let mut recorded_output_paths: BTreeSet<PathBuf> = BTreeSet::new();

    for dep_file in &dep_files {
        let Ok(text) = fs::read_to_string(dep_file) else {
            continue;
        };
        let stanzas = parse_all_stanzas(&text);
        for stanza in stanzas {
            let output_path = PathBuf::from(&stanza.output);
            // The `.d` file lists multiple outputs (rlib + rmeta + bin
            // depending on cargo's emit). We only stat outputs that exist
            // — cargo will routinely list outputs from prior runs that
            // were not actually produced this run.
            if output_path.exists() && !recorded_output_paths.contains(&output_path) {
                if let Ok(meta) = fs::metadata(&output_path) {
                    if let (Some(m), Some(s)) = (mtime_nanos(&meta), size_as_i64(&meta)) {
                        outputs.push(WorkspaceOutput {
                            path: output_path.to_string_lossy().to_string(),
                            mtime_nanos: m,
                            size_bytes: s,
                        });
                        recorded_output_paths.insert(output_path);
                    }
                }
            }
            for src in stanza.sources {
                let src = PathBuf::from(src);
                if !src.as_os_str().is_empty() {
                    sources.insert(src);
                }
            }
        }
    }

    // For `check`, additionally pick up every `.rmeta` file under
    // `<profile>/deps/` that doesn't already appear as an output — cargo
    // may write rmeta without an accompanying `.d` stanza naming it.
    if matches!(plan.verb, WorkspaceVerb::Check) {
        let deps_dir = profile_dir.join("deps");
        if let Ok(rd) = fs::read_dir(&deps_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("rmeta")
                    && !recorded_output_paths.contains(&path)
                {
                    if let Ok(meta) = fs::metadata(&path) {
                        if let (Some(m), Some(s)) = (mtime_nanos(&meta), size_as_i64(&meta)) {
                            outputs.push(WorkspaceOutput {
                                path: path.to_string_lossy().to_string(),
                                mtime_nanos: m,
                                size_bytes: s,
                            });
                            recorded_output_paths.insert(path);
                        }
                    }
                }
            }
        }
    }

    let mut source_entries: Vec<SidecarSource> = Vec::with_capacity(sources.len());
    for src in &sources {
        let Ok(meta) = fs::metadata(src) else {
            continue;
        };
        let Some(mtime) = mtime_nanos(&meta) else {
            continue;
        };
        let Some(size) = size_as_i64(&meta) else {
            continue;
        };
        source_entries.push(SidecarSource {
            path: src.to_string_lossy().to_string(),
            mtime_nanos: mtime,
            size_bytes: size,
        });
    }

    // Sort outputs for stable serialization.
    outputs.sort_by(|a, b| a.path.cmp(&b.path));
    source_entries.sort_by(|a, b| a.path.cmp(&b.path));

    let clippy_capture_entry = match (plan.verb, clippy_capture) {
        (WorkspaceVerb::Clippy, Some(capture)) => {
            let captures_dir = sidecar_path
                .parent()
                .ok_or_else(|| "sidecar has no parent dir".to_string())?
                .to_path_buf();
            fs::create_dir_all(&captures_dir)
                .map_err(|err| format!("mkdir {}: {err}", captures_dir.display()))?;
            let stdout_path = captures_dir.join("workspace-clippy.stdout.gz");
            let stderr_path = captures_dir.join("workspace-clippy.stderr.gz");
            write_gzip_file(&stdout_path, &capture.stdout)
                .map_err(|err| format!("write {}: {err}", stdout_path.display()))?;
            write_gzip_file(&stderr_path, &capture.stderr)
                .map_err(|err| format!("write {}: {err}", stderr_path.display()))?;
            Some(ClippyCaptureEntry {
                exit_code: capture.exit_code,
                stdout_path: stdout_path.to_string_lossy().to_string(),
                stderr_path: stderr_path.to_string_lossy().to_string(),
            })
        }
        _ => None,
    };

    let sidecar = WorkspaceSidecar {
        schema_version: WORKSPACE_SIDECAR_SCHEMA_VERSION,
        verb: plan.verb.label().to_string(),
        cargo_args_fingerprint: fingerprint,
        outputs,
        source_files: source_entries,
        clippy_capture: clippy_capture_entry,
    };

    write_workspace_sidecar_atomic(sidecar_path, &sidecar)
        .map_err(|err| format!("sidecar write failed: {} ({err})", sidecar_path.display()))?;
    Ok(sidecar_path.clone())
}

fn enumerate_dep_files(profile_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    push_dep_files_in_dir(profile_dir, &mut out);
    push_dep_files_in_dir(&profile_dir.join("deps"), &mut out);
    // examples/ and build/ subdirs would also produce .d files; pick up
    // examples for completeness. build scripts are not interesting for
    // freshness — their outputs are encoded in dep-info of their consumers.
    push_dep_files_in_dir(&profile_dir.join("examples"), &mut out);
    out
}

fn push_dep_files_in_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("d") {
            out.push(path);
        }
    }
}

/// Parse ALL stanzas from a `.d` file (the existing parser in
/// `trampoline_dep_info` returns a single stanza). Tolerant of malformed
/// input: skips lines we can't parse rather than failing outright.
struct DepStanza {
    output: String,
    sources: Vec<String>,
}

fn parse_all_stanzas(text: &str) -> Vec<DepStanza> {
    let logical = join_continuations(text);
    let mut stanzas = Vec::new();
    for line in &logical {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((lhs, rhs)) = split_dep_info_line(line) else {
            continue;
        };
        let sources = tokenize_dep_info_paths(&rhs);
        stanzas.push(DepStanza {
            output: lhs,
            sources,
        });
    }
    stanzas
}

fn join_continuations(text: &str) -> Vec<String> {
    let mut pending = String::new();
    let mut out = Vec::new();
    for raw in text.lines() {
        if let Some(stripped) = raw.strip_suffix('\\') {
            pending.push_str(stripped);
            pending.push(' ');
            continue;
        }
        pending.push_str(raw);
        out.push(std::mem::take(&mut pending));
    }
    if !pending.is_empty() {
        out.push(pending);
    }
    out
}

fn split_dep_info_line(line: &str) -> Option<(String, String)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if c == b':' && !is_drive_letter_colon(bytes, i) {
            return Some((
                line[..i].trim().to_string(),
                line[i + 1..].trim().to_string(),
            ));
        }
        i += 1;
    }
    None
}

fn is_drive_letter_colon(bytes: &[u8], i: usize) -> bool {
    if i + 1 >= bytes.len() {
        return false;
    }
    let separator = bytes[i + 1];
    if separator != b'\\' && separator != b'/' {
        return false;
    }
    let at_start = i == 1 && bytes[0].is_ascii_alphabetic();
    let after_space =
        i >= 2 && bytes[i - 2].is_ascii_whitespace() && bytes[i - 1].is_ascii_alphabetic();
    at_start || after_space
}

fn tokenize_dep_info_paths(rhs: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let bytes = rhs.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if matches!(next, b' ' | b'\t' | b'\\' | b'#' | b':') {
                let ch = if next == b':' { ':' } else { next as char };
                current.push(ch);
                i += 2;
                continue;
            }
            current.push('\\');
            i += 1;
            continue;
        }
        if c == b' ' || c == b'\t' {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            i += 1;
            continue;
        }
        current.push(c as char);
        i += 1;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Resolve the effective `<target-dir>/<target?>/<profile>/` directory.
fn compute_profile_dir(parsed: &ParsedRunArgs) -> PathBuf {
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
        if profile == "dev" {
            "debug".to_string()
        } else {
            profile.to_string()
        }
    } else {
        "debug".to_string()
    };
    leaf.push(&profile_dir);
    leaf
}

fn workspace_fell_through_plan(
    verb: WorkspaceVerb,
    parsed: Option<ParsedRunArgs>,
    cleaned_args: Vec<String>,
) -> WorkspaceFellThroughPlan {
    let (profile_dir, sidecar_path) = match parsed.as_ref() {
        Some(p) => {
            let dir = compute_profile_dir(p);
            let sidecar = dir.join(".soldr-trampoline").join(verb.sidecar_filename());
            (Some(dir), Some(sidecar))
        }
        None => (None, None),
    };
    WorkspaceFellThroughPlan {
        verb,
        parsed,
        cleaned_args,
        profile_dir,
        sidecar_path,
    }
}

// ---------------------------------------------------------------------------
// Sidecar schema
// ---------------------------------------------------------------------------

pub(crate) const WORKSPACE_SIDECAR_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkspaceSidecar {
    schema_version: u32,
    verb: String,
    cargo_args_fingerprint: String,
    #[serde(default)]
    outputs: Vec<WorkspaceOutput>,
    #[serde(default, rename = "source_files")]
    source_files: Vec<SidecarSource>,
    #[serde(default)]
    clippy_capture: Option<ClippyCaptureEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkspaceOutput {
    path: String,
    mtime_nanos: i64,
    size_bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ClippyCaptureEntry {
    exit_code: i32,
    stdout_path: String,
    stderr_path: String,
}

fn write_workspace_sidecar_atomic(path: &Path, data: &WorkspaceSidecar) -> std::io::Result<()> {
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
// Gzip helpers for clippy capture
// ---------------------------------------------------------------------------

fn write_gzip_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp_name: OsString = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    {
        let file = fs::File::create(&tmp)?;
        let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        enc.write_all(data)?;
        enc.finish()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn read_gzip_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut dec = flate2::read::GzDecoder::new(file);
    let mut buf = Vec::new();
    dec.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
#[path = "trampoline_workspace_tests.rs"]
mod tests;
