//! `soldr optimize` — platform-aware hot-cache optimization. See
//! issue #358 for the design rationale.
//!
//! On Windows this adds the soldr-owned cache directories (and the
//! current project's `target/`) to Windows Defender's real-time
//! scanning exclusion list, falling back to UAC self-relaunch when the
//! current process is not elevated. On macOS / Linux the subcommand is
//! a no-op with a clear message.

use crate::cache_lib::zccache_dir;
use crate::core::{SoldrError, SoldrPaths, SOLDR_CACHE_DIR_ENV_VAR};
use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cache::print_json;
use crate::JSON_SCHEMA_VERSION;

use crate::defender::{apply_exclusions, current_exclusion_list, is_admin};
use crate::optimize_windows::{relaunch_elevated, ELEVATED_HELPER_FLAG};

use crate::defender::find_powershell;
use crate::optimize_detect::{
    detect_ci, detect_platform, detect_tools, Platform, ToolDetectionMode,
};

/// Filename of the soldr-owned tracking file recording which paths
/// soldr added to Defender's exclusion list. Stored in `~/.soldr/`.
const MANAGED_EXCLUSIONS_FILE: &str = "managed-defender-exclusions.json";

/// `--scope` accepted values. Maps to the four-way enum described in
/// issue #358.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub(crate) enum OptimizeScope {
    /// `~/.soldr/cache`, `~/.soldr/bench`, `~/.soldr/runtime`,
    /// `~/.soldr/state.sqlite3`, and the resolved zccache cache dir.
    Global,
    /// `<workspace_root>/target` (walks up from cwd for `Cargo.toml`).
    Project,
    /// Apply both `global` and `project` scopes.
    All,
}

impl OptimizeScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::All => "all",
        }
    }
}

/// Top-level argument struct for `clap`.
#[derive(Debug, Args)]
pub(crate) struct OptimizeArgs {
    /// What to optimize. Defaults to `all`.
    #[arg(long, value_enum, default_value_t = OptimizeScope::All)]
    pub(crate) scope: OptimizeScope,
    /// Reverse soldr-added exclusions for the chosen scope. Never
    /// touches user-added entries.
    #[arg(long)]
    pub(crate) undo: bool,
    /// Print what would change and exit without invoking PowerShell.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub(crate) json: bool,
    /// Path to an explicit `Cargo.toml` for the `project` scope. When
    /// unset, soldr walks up from cwd.
    #[arg(long, value_name = "PATH")]
    pub(crate) manifest_path: Option<PathBuf>,
    /// Internal: this process was launched by a parent soldr via UAC
    /// self-relaunch. Skips re-elevation and writes JSON status to
    /// the path in `SOLDR_OPTIMIZE_HELPER_OUTPUT`.
    #[arg(long = "as-elevated-helper", hide = true)]
    pub(crate) as_elevated_helper: bool,
}

/// Tracking JSON written to `~/.soldr/managed-defender-exclusions.json`.
/// Schema is versioned so future migrations don't silently corrupt
/// the file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct ManagedExclusionFile {
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) exclusions: Vec<ManagedExclusion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ManagedExclusion {
    pub(crate) path: String,
    pub(crate) added_at_unix: u64,
    pub(crate) scope: String,
}

pub(crate) use crate::defender::{ActionStatus, ExclusionAction, PathAction};

/// Top-level JSON shape returned by `soldr optimize --json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct OptimizeOutput {
    pub(crate) schema_version: u32,
    pub(crate) command: String,
    pub(crate) platform: String,
    pub(crate) scope: String,
    pub(crate) undo: bool,
    pub(crate) dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ci_label: Option<String>,
    pub(crate) defender_present: bool,
    pub(crate) defender_active: bool,
    pub(crate) actions: Vec<PathAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<String>,
}

impl OptimizeOutput {
    /// Sample value used by JSON round-trip tests.
    #[cfg(test)]
    pub(crate) fn sample() -> Self {
        Self {
            schema_version: JSON_SCHEMA_VERSION,
            command: "optimize".into(),
            platform: "Windows10".into(),
            scope: "global".into(),
            undo: false,
            dry_run: true,
            ci_label: None,
            defender_present: true,
            defender_active: true,
            actions: vec![PathAction {
                path: "C:\\Users\\demo\\.soldr\\cache".into(),
                action: ExclusionAction::Add,
                scope: "global".into(),
                status: ActionStatus::Planned,
                detail: None,
            }],
            note: Some("dry run".into()),
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve the project's `target/` directory by walking up from
/// `start_dir` until a `Cargo.toml` is found. Honors the explicit
/// `manifest_path` override when provided.
pub(crate) fn resolve_project_target_dir(
    start_dir: &Path,
    manifest_path: Option<&Path>,
) -> Result<PathBuf, SoldrError> {
    if let Some(manifest) = manifest_path {
        let dir = manifest.parent().ok_or_else(|| {
            SoldrError::Other(format!(
                "--manifest-path {} has no parent directory",
                manifest.display()
            ))
        })?;
        return Ok(dir.join("target"));
    }
    let mut current = start_dir.to_path_buf();
    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file() {
            return Ok(current.join("target"));
        }
        if !current.pop() {
            return Err(SoldrError::Other(format!(
                "no Rust project detected from {}; pass --manifest-path to point at a Cargo.toml",
                start_dir.display()
            )));
        }
    }
}

/// Compute the paths covered by `--scope global`.
pub(crate) fn plan_global_paths(soldr_root: &Path, resolved_zccache_dir: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        soldr_root.join("cache"),
        soldr_root.join("bench"),
        soldr_root.join("runtime"),
        soldr_root.join("state.sqlite3"),
    ];
    let zccache = resolved_zccache_dir.to_path_buf();
    if !paths.iter().any(|p| p == &zccache) {
        paths.push(zccache);
    }
    paths
}

/// Compute the paths covered by `--scope project`.
pub(crate) fn plan_project_paths(workspace_root: &Path) -> Vec<PathBuf> {
    vec![workspace_root.join("target")]
}

/// Compute the set of paths to `Remove-MpPreference` during an undo
/// flow. Only soldr-tracked entries are returned; user-added Defender
/// exclusions are never touched.
pub(crate) fn filter_undo_entries(
    managed: &ManagedExclusionFile,
    _current_defender_list: &[String],
    scope: Option<OptimizeScope>,
) -> Vec<String> {
    managed
        .exclusions
        .iter()
        .filter(|e| match scope {
            None => true,
            Some(OptimizeScope::All) => true,
            Some(OptimizeScope::Global) => e.scope == "global",
            Some(OptimizeScope::Project) => e.scope == "project",
        })
        .map(|e| e.path.clone())
        .collect()
}

fn load_managed_file(path: &Path) -> ManagedExclusionFile {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ManagedExclusionFile {
            schema_version: 1,
            exclusions: Vec::new(),
        };
    };
    serde_json::from_str(&text).unwrap_or(ManagedExclusionFile {
        schema_version: 1,
        exclusions: Vec::new(),
    })
}

fn save_managed_file(path: &Path, file: &ManagedExclusionFile) -> Result<(), SoldrError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(SoldrError::Io)?;
    }
    let text = serde_json::to_string_pretty(file)
        .map_err(|e| SoldrError::Other(format!("failed to serialize managed exclusions: {e}")))?;
    std::fs::write(path, text).map_err(SoldrError::Io)
}

fn platform_label(platform: Platform) -> &'static str {
    match platform {
        Platform::Windows10 => "Windows10",
        Platform::Windows11Pre22H2 => "Windows11Pre22H2",
        Platform::Windows11Post22H2 => "Windows11Post22H2",
        Platform::MacOS => "macOS",
        Platform::Linux => "Linux",
        Platform::Other => "Other",
    }
}

/// Build a `PathAction` for each path with `ActionStatus::Planned`.
fn build_plan(
    scope_for_paths: &[(OptimizeScope, Vec<PathBuf>)],
    action: ExclusionAction,
) -> Vec<PathAction> {
    let mut out = Vec::new();
    for (scope, paths) in scope_for_paths {
        for path in paths {
            out.push(PathAction {
                path: path.display().to_string(),
                action,
                scope: scope.as_str().into(),
                status: ActionStatus::Planned,
                detail: None,
            });
        }
    }
    out
}

/// Entry point for `soldr optimize ...`. Returns the process exit code.
pub(crate) fn run_optimize(args: OptimizeArgs) -> Result<i32, SoldrError> {
    let platform = detect_platform();
    let scope = args.scope;
    let ci_label = detect_ci();

    let paths = SoldrPaths::new()?;
    let zccache_path = zccache_dir(&paths);
    let zccache_overridden = std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR)
        .is_some()
        && std::env::var_os(crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR).is_none();

    // CI auto-skip.
    if let Some(label) = ci_label {
        let mut output = base_output(platform, scope, args.undo, args.dry_run);
        output.ci_label = Some(label.to_string());
        output.note = Some(format!(
            "running in CI ({label}); ephemeral runner image, exclusions would be discarded. Skipping."
        ));
        emit_output(&output, args.json)?;
        return Ok(0);
    }

    // Non-Windows platforms are no-ops.
    if !matches!(
        platform,
        Platform::Windows10 | Platform::Windows11Pre22H2 | Platform::Windows11Post22H2
    ) {
        let mut output = base_output(platform, scope, args.undo, args.dry_run);
        output.note = Some(match platform {
            Platform::MacOS => {
                "macOS does not need cache exclusions for soldr's workloads. Exiting.".into()
            }
            Platform::Linux => {
                "Linux does not need cache exclusions for soldr's workloads. Exiting.".into()
            }
            _ => "Unsupported platform; optimize is a no-op.".into(),
        });
        emit_output(&output, args.json)?;
        return Ok(0);
    }

    let tools = detect_tools(
        platform,
        if args.dry_run {
            ToolDetectionMode::DryRun
        } else {
            ToolDetectionMode::Live
        },
    );

    if !args.dry_run && !tools.defender_present {
        let mut output = base_output(platform, scope, args.undo, args.dry_run);
        output.defender_present = false;
        output.defender_active = false;
        output.note = Some("Defender not active; no exclusions needed.".into());
        emit_output(&output, args.json)?;
        return Ok(0);
    }

    let powershell = if args.dry_run {
        PathBuf::new()
    } else {
        let Some(powershell) = tools.powershell.clone().or_else(find_powershell) else {
            return Err(SoldrError::Other(
                "PowerShell not found on PATH. Install PowerShell (pwsh or powershell.exe) or run the helper manually.".into(),
            ));
        };
        powershell
    };

    // Build per-scope path lists.
    let mut plan_paths: Vec<(OptimizeScope, Vec<PathBuf>)> = Vec::new();
    let want_global = matches!(scope, OptimizeScope::Global | OptimizeScope::All);
    let want_project = matches!(scope, OptimizeScope::Project | OptimizeScope::All);
    let mut warnings: Vec<String> = Vec::new();
    if want_global {
        plan_paths.push((
            OptimizeScope::Global,
            plan_global_paths(&paths.root, &zccache_path),
        ));
        if zccache_overridden {
            warnings.push(format!(
                "ZCCACHE_CACHE_DIR is set externally; resolved zccache dir ({}) is being excluded explicitly. Unset {} to revert to soldr's managed location unless you have a specific reason.",
                zccache_path.display(),
                crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
            ));
        }
        if std::env::var_os(SOLDR_CACHE_DIR_ENV_VAR).is_some() {
            warnings.push(format!(
                "{} is set; soldr's root resolved to {} — exclusions follow accordingly.",
                SOLDR_CACHE_DIR_ENV_VAR,
                paths.root.display(),
            ));
        }
    }
    if want_project {
        let workspace_root = if let Some(manifest) = args.manifest_path.as_deref() {
            manifest
                .parent()
                .ok_or_else(|| {
                    SoldrError::Other(format!(
                        "--manifest-path {} has no parent directory",
                        manifest.display()
                    ))
                })?
                .to_path_buf()
        } else {
            let start = std::env::current_dir().map_err(SoldrError::from)?;
            let target = resolve_project_target_dir(&start, None)?;
            target
                .parent()
                .ok_or_else(|| {
                    SoldrError::Other(format!(
                        "resolved project target {} has no parent",
                        target.display()
                    ))
                })?
                .to_path_buf()
        };
        plan_paths.push((OptimizeScope::Project, plan_project_paths(&workspace_root)));
    }

    // Undo flow short-circuits the add/plan logic.
    let managed_path = paths.root.join(MANAGED_EXCLUSIONS_FILE);
    if args.undo {
        let managed = load_managed_file(&managed_path);
        let scope_filter = match scope {
            OptimizeScope::All => None,
            other => Some(other),
        };
        let existing = if args.dry_run {
            Vec::new()
        } else {
            current_exclusion_list(&powershell)
        };
        let to_remove = filter_undo_entries(&managed, &existing, scope_filter);

        let plan: Vec<PathAction> = to_remove
            .iter()
            .map(|path| PathAction {
                path: path.clone(),
                action: ExclusionAction::Remove,
                scope: managed
                    .exclusions
                    .iter()
                    .find(|e| e.path == *path)
                    .map(|e| e.scope.clone())
                    .unwrap_or_else(|| "unknown".into()),
                status: ActionStatus::Planned,
                detail: None,
            })
            .collect();

        let mut output = base_output(platform, scope, args.undo, args.dry_run);
        output.defender_present = tools.defender_present;
        output.defender_active = tools.defender_active;

        if args.dry_run {
            output.actions = plan;
            output.note = Some(format!(
                "dry-run: {} entries would be removed",
                output.actions.len()
            ));
            emit_output(&output, args.json)?;
            return Ok(0);
        }

        if !is_admin() {
            return elevate_or_explain(&powershell, &paths.root, args.json, output, true);
        }

        let applied = apply_exclusions(&powershell, &plan, &existing);

        // Prune successfully removed entries from the managed file.
        let mut new_managed = managed.clone();
        new_managed.exclusions.retain(|e| {
            applied
                .iter()
                .find(|a| a.path == e.path)
                .is_none_or(|a| !matches!(a.status, ActionStatus::Applied | ActionStatus::Skipped))
        });
        save_managed_file(&managed_path, &new_managed)?;

        output.actions = applied;
        if !warnings.is_empty() {
            output.note = Some(warnings.join("; "));
        }
        emit_output(&output, args.json)?;
        return Ok(0);
    }

    // ----- Add/optimize flow -----
    let plan: Vec<PathAction> = build_plan(&plan_paths, ExclusionAction::Add);

    let mut output = base_output(platform, scope, args.undo, args.dry_run);
    output.defender_present = tools.defender_present;
    output.defender_active = tools.defender_active;

    if args.dry_run {
        output.actions = plan;
        let mut notes = warnings;
        notes.push(format!(
            "dry-run: would add {} paths to Defender exclusions",
            output.actions.len()
        ));
        if matches!(platform, Platform::Windows11Post22H2) && tools.fsutil_devdrv_supported {
            notes.push(dev_drive_suggestion());
        } else if matches!(platform, Platform::Windows11Pre22H2) {
            notes.push("Dev Drive becomes available in Windows 11 22H2.".into());
        }
        output.note = Some(notes.join("; "));
        emit_output(&output, args.json)?;
        return Ok(0);
    }

    if !is_admin() {
        return elevate_or_explain(&powershell, &paths.root, args.json, output, false);
    }

    let existing = current_exclusion_list(&powershell);
    let applied = apply_exclusions(&powershell, &plan, &existing);

    // Persist what we just added.
    let mut managed = load_managed_file(&managed_path);
    let now = now_unix();
    for action in &applied {
        if matches!(action.status, ActionStatus::Applied)
            && !managed.exclusions.iter().any(|e| e.path == action.path)
        {
            managed.exclusions.push(ManagedExclusion {
                path: action.path.clone(),
                added_at_unix: now,
                scope: action.scope.clone(),
            });
        }
    }
    if managed.schema_version == 0 {
        managed.schema_version = 1;
    }
    save_managed_file(&managed_path, &managed)?;

    output.actions = applied;
    let mut notes = warnings;
    if matches!(platform, Platform::Windows11Post22H2) && tools.fsutil_devdrv_supported {
        notes.push(dev_drive_suggestion());
    } else if matches!(platform, Platform::Windows11Pre22H2) {
        notes.push("Dev Drive becomes available in Windows 11 22H2.".into());
    }
    if !notes.is_empty() {
        output.note = Some(notes.join("; "));
    }

    emit_output(&output, args.json)?;
    Ok(0)
}

fn dev_drive_suggestion() -> String {
    "For maximum performance, consider creating a Dev Drive and pointing SOLDR_CACHE_DIR at it — see https://learn.microsoft.com/en-us/windows/dev-drive/".into()
}

fn base_output(
    platform: Platform,
    scope: OptimizeScope,
    undo: bool,
    dry_run: bool,
) -> OptimizeOutput {
    OptimizeOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "optimize".into(),
        platform: platform_label(platform).to_string(),
        scope: scope.as_str().to_string(),
        undo,
        dry_run,
        ci_label: None,
        defender_present: false,
        defender_active: false,
        actions: Vec::new(),
        note: None,
    }
}

fn emit_output(output: &OptimizeOutput, json: bool) -> Result<(), SoldrError> {
    if json {
        print_json(output)?;
    } else {
        print_human(output);
    }
    Ok(())
}

fn print_human(output: &OptimizeOutput) {
    println!("soldr optimize: platform={}", output.platform);
    println!("  scope: {}", output.scope);
    if output.undo {
        println!("  mode: undo");
    } else if output.dry_run {
        println!("  mode: dry-run");
    } else {
        println!("  mode: apply");
    }
    if let Some(label) = &output.ci_label {
        println!("  ci: {label}");
    }
    println!(
        "  defender: present={}, active={}",
        output.defender_present, output.defender_active
    );
    if output.actions.is_empty() {
        println!("  (no actions)");
    } else {
        for action in &output.actions {
            let verb = match action.action {
                ExclusionAction::Add => "add",
                ExclusionAction::Remove => "remove",
            };
            let status = match action.status {
                ActionStatus::Planned => "would",
                ActionStatus::Applied => "ok",
                ActionStatus::AlreadyApplied => "already",
                ActionStatus::Skipped => "skip",
                ActionStatus::Failed => "FAIL",
            };
            if let Some(detail) = &action.detail {
                println!("  [{status}] {verb}: {} ({detail})", action.path);
            } else {
                println!("  [{status}] {verb}: {}", action.path);
            }
        }
    }
    if let Some(note) = &output.note {
        println!("  note: {note}");
    }
}

/// On non-admin invocations: attempt UAC self-relaunch on Windows; on
/// failure (or non-Windows targets that somehow reach this branch),
/// emit instructions and exit non-zero.
fn elevate_or_explain(
    powershell: &Path,
    soldr_root: &Path,
    json: bool,
    mut output: OptimizeOutput,
    undo: bool,
) -> Result<i32, SoldrError> {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        let helper_output_path =
            soldr_root.join(format!(".soldr-optimize-helper-{}.json", now_unix()));
        let argv = rebuild_argv_for_helper(&output, undo);
        match relaunch_elevated(powershell, &argv, &helper_output_path) {
            Ok(code) => {
                // Read the helper's JSON output if present and propagate.
                if let Ok(text) = std::fs::read_to_string(&helper_output_path) {
                    let _ = std::fs::remove_file(&helper_output_path);
                    if let Ok(helper_output) = serde_json::from_str::<OptimizeOutput>(&text) {
                        emit_output(&helper_output, json)?;
                        return Ok(code);
                    }
                }
                output.note = Some(format!(
                    "elevated helper exited with code {code} and produced no parseable status. \
                     Likely cause: UAC was declined or the helper crashed before writing output. \
                     Fallback: open an Administrator PowerShell and run \
                     `powershell -ExecutionPolicy Bypass -File bench\\add_defender_exclusions.ps1`, \
                     or invoke `Add-MpPreference -ExclusionPath '<path>'` manually for each soldr cache path."
                ));
                emit_output(&output, json)?;
                Ok(code)
            }
            Err(err) => {
                output.note = Some(format!(
                    "UAC self-relaunch failed: {err}. \
                     Fallback options: (1) re-run `soldr optimize` and accept the UAC prompt; \
                     (2) open an Administrator PowerShell and run \
                     `powershell -ExecutionPolicy Bypass -File bench\\add_defender_exclusions.ps1`; \
                     (3) add the exclusions manually with `Add-MpPreference -ExclusionPath '<path>'` \
                     for each soldr cache directory."
                ));
                emit_output(&output, json)?;
                Ok(1)
            }
        }
    } else {
        output.note = Some(
            "Administrator privileges required, but UAC self-relaunch is only available on Windows. \
             On macOS/Linux this code path is unreachable in normal flow -- file an issue if you hit it."
                .into(),
        );
        emit_output(&output, json)?;
        Ok(1)
    }
}

fn rebuild_argv_for_helper(output: &OptimizeOutput, undo: bool) -> Vec<String> {
    let mut argv = vec![
        "optimize".into(),
        "--scope".into(),
        output.scope.clone(),
        "--json".into(),
        ELEVATED_HELPER_FLAG.into(),
    ];
    if undo {
        argv.push("--undo".into());
    }
    argv
}

/// Entry point for `soldr defender-exclusions ...` (issue #355).
///
/// Thin re-skin of `run_optimize` with self-documenting verbs. Each
/// verb maps onto the existing optimize machinery:
///
/// * `check`  → `--scope all --dry-run` (no admin required)
/// * `add`    → `--scope all` (requires admin; UAC self-relaunches)
/// * `remove` → `--scope all --undo` (requires admin)
///
/// Sharing the implementation keeps a single source of truth for the
/// Defender path taxonomy, CI auto-skip, and UAC handshake.
pub(crate) fn run_defender_exclusions(
    sub: crate::DefenderExclusionsSubcommand,
) -> Result<i32, SoldrError> {
    use crate::DefenderExclusionsSubcommand;
    let args = match sub {
        DefenderExclusionsSubcommand::Check { json } => OptimizeArgs {
            scope: OptimizeScope::All,
            undo: false,
            dry_run: true,
            json,
            manifest_path: None,
            as_elevated_helper: false,
        },
        DefenderExclusionsSubcommand::Add { json, dry_run } => OptimizeArgs {
            scope: OptimizeScope::All,
            undo: false,
            dry_run,
            json,
            manifest_path: None,
            as_elevated_helper: false,
        },
        DefenderExclusionsSubcommand::Remove { json, dry_run } => OptimizeArgs {
            scope: OptimizeScope::All,
            undo: true,
            dry_run,
            json,
            manifest_path: None,
            as_elevated_helper: false,
        },
    };
    run_optimize(args)
}

#[cfg(test)]
#[path = "optimize_tests.rs"]
mod tests;
