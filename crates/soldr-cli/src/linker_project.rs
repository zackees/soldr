//! Project-level `reld`/linker choice resolution (soldr#3276 §4).
//!
//! Split out of `linker.rs` to stay under the per-file line ceiling: this
//! module is the "one shared resolver" precedence chain (env >
//! `.cargo/config.toml` > `Cargo.toml` metadata > `~/.soldr/config.toml` >
//! default) consumed by the cargo front door, `soldr build`, the PEP 517
//! path, and `soldr prepare` / `soldr toolchain prepare|ensure` alike.

use crate::core::{SoldrError, SoldrPaths};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::linker::{extract_flag_value, project_root, target_config_value_in_files, LinkerChoice};

/// Where a resolved project linker choice came from, highest precedence
/// first: `Env` > `CargoConfig` > `CargoTomlMetadata` > `UserConfig` >
/// `Default`. Section 4 of soldr#3276 — one shared resolver so `soldr build`,
/// `soldr cargo`, and the PEP 517 front door all read the same precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkerSource {
    Env,
    CargoConfig,
    CargoTomlMetadata,
    UserConfig,
    Default,
}

/// How a project's own `.cargo/config.toml` (or `$CARGO_HOME/config.toml`)
/// named `reld` as a bare linker, when it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldCargoConfig {
    /// `[target.<triple>] linker = "reld"` (or `"reld.exe"`).
    BareLinker,
    /// `[target.<triple>] rustflags` contains a bare `--ld-path=reld` or
    /// `linker=reld` fragment.
    Rustflags,
}

/// The resolved project-level linker choice plus where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLinkerSelection {
    pub choice: LinkerChoice,
    pub source: LinkerSource,
    /// `Some` when the choice traces back to a project/cargo_home cargo
    /// config declaring a bare `reld`, and which shape it was declared as.
    pub reld_cargo_config: Option<ReldCargoConfig>,
}

impl ProjectLinkerSelection {
    /// Whether a source other than the compiled-in default made this choice.
    pub fn is_explicit(&self) -> bool {
        self.source != LinkerSource::Default
    }

    /// Whether resolving this selection into an injection requires a
    /// verified `reld` executable path first.
    pub fn needs_reld(&self) -> bool {
        self.choice == LinkerChoice::Reld || self.reld_cargo_config.is_some()
    }
}

/// Whether a `[target.<triple>] linker` value names `reld` as a bare
/// command (resolved via `PATH`), not an absolute path. soldr must not
/// overwrite an absolute-path reld linker a project already pinned itself.
fn is_bare_reld_linker(value: &str) -> bool {
    matches!(value.trim(), "reld" | "reld.exe")
}

/// Whether a `[target.<triple>] rustflags` value drives a bare `reld` via
/// `--ld-path=reld` or `linker=reld` (not an absolute path to either).
fn rustflags_declares_bare_reld(rustflags: &str) -> bool {
    matches!(extract_flag_value(rustflags, "--ld-path="), Some("reld"))
        || matches!(extract_flag_value(rustflags, "linker="), Some("reld"))
}

/// Resolve the project-level linker choice from (in precedence order,
/// highest first):
///
/// 1. `env` (`SOLDR_LINKER`) — an invalid value is a hard error.
/// 2. `[target.<triple>]` (or the universally-matching
///    `[target.'cfg(all())']`) in `project_root/.cargo/config.toml` (or the
///    legacy `.cargo/config`), then `cargo_home/config.toml` (project config
///    wins when both declare the target). A bare `reld` linker or rustflags
///    fragment resolves to `LinkerChoice::Reld`; any other declared
///    linker/rustflags resolves to `LinkerChoice::Default` so soldr does not
///    override a project's own choice (soldr#3277) — including an
///    absolute-path `reld`, which is left alone rather than re-resolved.
///    Dylint-scoped callers layer their lint libraries' configs ahead of
///    the project root's via [`resolve_project_choice_with_lint_roots`]
///    (soldr#3483).
/// 3. `[workspace.metadata.soldr].linker` / `[package.metadata.soldr].linker`
///    from `project_root/Cargo.toml` — an invalid value is a hard error. A
///    missing `Cargo.toml` is treated as "not declared", not an error.
/// 4. `user_config` (`~/.soldr/config.toml` `linker = "..."`).
/// 5. Otherwise `LinkerChoice::Fast` (reld is the default linker,
///    soldr#3262).
pub fn resolve_project_choice(
    env: Option<&OsStr>,
    user_config: Option<&str>,
    target: Option<&str>,
    project_root: &Path,
    cargo_home: Option<&Path>,
) -> Result<ProjectLinkerSelection, SoldrError> {
    resolve_project_choice_with_lint_roots(env, user_config, target, project_root, cargo_home, &[])
}

/// [`resolve_project_choice`] plus Dylint library roots (soldr#3483).
///
/// `lint_roots` are the workspace's declared Dylint library package
/// directories (see [`crate::dylint_libraries::library_directories`]). A
/// lint crate declares its linker in *its own* `.cargo/config.toml` —
/// `[target.'cfg(all())'] linker = "dylint-link"` — which Cargo reads for
/// the lint build (whose cwd is the lint package root) but which the
/// workspace-root lookup below never sees from an outer `cargo dylint`
/// invocation. The lint roots are consulted **before** the project root
/// because they are the config closest to the lint build, matching Cargo's
/// closest-config-wins merge. Callers pass an empty slice outside a
/// Dylint-scoped invocation so a plain build never consults a lint
/// package's config (Cargo would not either).
pub fn resolve_project_choice_with_lint_roots(
    env: Option<&OsStr>,
    user_config: Option<&str>,
    target: Option<&str>,
    project_root: &Path,
    cargo_home: Option<&Path>,
    lint_roots: &[PathBuf],
) -> Result<ProjectLinkerSelection, SoldrError> {
    if let Some(env) = env {
        let env = env
            .to_str()
            .ok_or_else(|| SoldrError::Other("SOLDR_LINKER is not valid UTF-8".to_string()))?;
        let choice = LinkerChoice::from_str(env)?;
        return Ok(ProjectLinkerSelection {
            choice,
            source: LinkerSource::Env,
            reld_cargo_config: None,
        });
    }

    if let Some(target) = target {
        let mut config_files = Vec::with_capacity(2 + 2 * lint_roots.len() + 2);
        // soldr#3483: a Dylint library's own `.cargo/config.toml` first —
        // it is the config the lint build actually resolves.
        for lint_root in lint_roots {
            config_files.push(lint_root.join(".cargo/config.toml"));
            config_files.push(lint_root.join(".cargo/config"));
        }
        config_files.push(project_root.join(".cargo/config.toml"));
        config_files.push(project_root.join(".cargo/config"));
        if let Some(cargo_home) = cargo_home {
            config_files.push(cargo_home.join("config.toml"));
            config_files.push(cargo_home.join("config"));
        }
        let linker_value = target_config_value_in_files(&config_files, target, "linker");
        let rustflags_value = target_config_value_in_files(&config_files, target, "rustflags");

        if linker_value.as_deref().is_some_and(is_bare_reld_linker) {
            return Ok(ProjectLinkerSelection {
                choice: LinkerChoice::Reld,
                source: LinkerSource::CargoConfig,
                reld_cargo_config: Some(ReldCargoConfig::BareLinker),
            });
        }
        if rustflags_value
            .as_deref()
            .is_some_and(rustflags_declares_bare_reld)
        {
            return Ok(ProjectLinkerSelection {
                choice: LinkerChoice::Reld,
                source: LinkerSource::CargoConfig,
                reld_cargo_config: Some(ReldCargoConfig::Rustflags),
            });
        }
        if linker_value.is_some() || rustflags_value.is_some() {
            // soldr#3277: something is declared (including an absolute-path
            // reld) but it is not a bare reld we can resolve -- decline to
            // inject anything so the project's own setting stands.
            return Ok(ProjectLinkerSelection {
                choice: LinkerChoice::Default,
                source: LinkerSource::CargoConfig,
                reld_cargo_config: None,
            });
        }
    }

    let cargo_toml = project_root.join("Cargo.toml");
    if cargo_toml.is_file() {
        let metadata = crate::cargo_metadata_soldr::read_soldr_metadata(&cargo_toml)?;
        if let Some(value) = metadata.linker {
            let choice = LinkerChoice::from_str(&value)?;
            return Ok(ProjectLinkerSelection {
                choice,
                source: LinkerSource::CargoTomlMetadata,
                reld_cargo_config: None,
            });
        }
    }

    if let Some(user_config) = user_config {
        let choice = LinkerChoice::from_str(user_config)?;
        return Ok(ProjectLinkerSelection {
            choice,
            source: LinkerSource::UserConfig,
            reld_cargo_config: None,
        });
    }

    Ok(ProjectLinkerSelection {
        choice: LinkerChoice::Fast,
        source: LinkerSource::Default,
        reld_cargo_config: None,
    })
}

/// [`resolve_project_choice`] driven from process-ambient state: the current
/// working directory locates the project root, `SOLDR_LINKER` is read
/// directly, `paths.load_config()` supplies the user config, and
/// `CARGO_HOME` (falling back to `~/.cargo`) supplies the cargo-home config
/// fallback.
///
/// `dylint_active` additionally layers the workspace's declared Dylint
/// library configs ahead of the project root's (soldr#3483) — see
/// [`resolve_project_choice_with_lint_roots`].
pub fn resolve_project_choice_from_cwd(
    target: Option<&str>,
    paths: &SoldrPaths,
    dylint_active: bool,
) -> Result<ProjectLinkerSelection, SoldrError> {
    let env = std::env::var_os(crate::LINKER_ENV_VAR);
    let config = paths
        .load_config()
        .map_err(|error| SoldrError::Other(error.to_string()))?;
    let cwd = std::env::current_dir()
        .map_err(|error| SoldrError::Other(format!("soldr: current directory: {error}")))?;
    let root = project_root(&cwd);
    let cargo_home = crate::core::resolve_cargo_home();
    // Best-effort: a workspace whose Cargo.toml cannot even be walked for
    // declared libraries must not fail the linker resolution — the
    // workspace-root config lookup below still runs.
    let lint_roots = if dylint_active {
        crate::dylint_libraries::library_directories(&root).unwrap_or_default()
    } else {
        Vec::new()
    };

    resolve_project_choice_with_lint_roots(
        env.as_deref(),
        config.linker.as_deref(),
        target,
        &root,
        cargo_home.as_deref(),
        &lint_roots,
    )
}

/// Wrap a failed `ensure_reld` for an *explicit* reld selection into an
/// actionable error: name the managed version, the release URL prefix, and
/// the escape hatch env var so the message stands alone in a build log.
pub fn reld_fetch_error(err: SoldrError) -> SoldrError {
    SoldrError::Other(format!(
        "soldr: could not resolve the reld linker (managed version {}, \
         releases at https://github.com/zackees/reld/releases): {err}. \
         Set {} to an absolute path to an existing reld executable to bypass \
         the managed download.",
        crate::fetch::MANAGED_RELD_VERSION,
        crate::fetch::RELD_BIN_ENV_VAR,
    ))
}
