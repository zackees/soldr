//! Source-checkout preflight for the pinned zccache submodule.

use std::fs;
use std::path::{Path, PathBuf};

use crate::core::SoldrError;

const MANIFEST: &str = concat!("C", "argo.toml");

pub(crate) fn ensure_zccache_submodule_initialized(args: &[String]) -> Result<(), SoldrError> {
    let cwd = std::env::current_dir().map_err(|error| {
        SoldrError::Other(format!(
            "could not determine the current directory: {error}"
        ))
    })?;
    if let Some(root) = missing_zccache_submodule(&cwd, args) {
        return Err(SoldrError::Other(format!(
            "Soldr source checkout at {} is missing its vendored zccache submodule; run: git submodule update --init _vender/zccache",
            root.display()
        )));
    }
    Ok(())
}

fn missing_zccache_submodule(cwd: &Path, args: &[String]) -> Option<PathBuf> {
    cwd.ancestors().find_map(|candidate| {
        (is_soldr_source_checkout(candidate)
            && cargo_invocation_targets_checkout(args, cwd, candidate)
            && !candidate.join("_vender/zccache").join(MANIFEST).is_file())
        .then(|| candidate.to_path_buf())
    })
}

fn cargo_invocation_targets_checkout(args: &[String], cwd: &Path, root: &Path) -> bool {
    if let Some(manifest) = option_path(args, "--manifest-path") {
        return path_is_within_checkout(&manifest, cwd, root);
    }

    let subcommand = crate::cargo_front_door::first_cargo_subcommand(args);
    if subcommand == Some("install") {
        return option_path(args, "--path")
            .is_some_and(|path| path_is_within_checkout(&path, cwd, root));
    }

    !matches!(
        subcommand,
        None | Some(
            "help"
                | "init"
                | "login"
                | "logout"
                | "locate-project"
                | "new"
                | "owner"
                | "search"
                | "uninstall"
                | "yank"
        )
    )
}

fn path_is_within_checkout(path: &Path, cwd: &Path, root: &Path) -> bool {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    path.canonicalize()
        .and_then(|path| root.canonicalize().map(|root| path.starts_with(root)))
        .unwrap_or(false)
}

fn option_path(args: &[String], option: &str) -> Option<PathBuf> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == option {
            return args.next().map(PathBuf::from);
        }
        if let Some(path) = arg.strip_prefix(&format!("{option}=")) {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn is_soldr_source_checkout(candidate: &Path) -> bool {
    let root = fs::read_to_string(candidate.join(MANIFEST)).ok();
    let cli = fs::read_to_string(candidate.join("crates/soldr-cli").join(MANIFEST)).ok();
    let modules = fs::read_to_string(candidate.join(".gitmodules")).ok();
    matches!(
        (root, cli, modules),
        (Some(root), Some(cli), Some(modules))
            if root.contains("repository = \"https://github.com/zackees/soldr\"")
                && cli.contains("name = \"soldr-cli\"")
                && modules.lines().any(|line| line.trim() == "path = _vender/zccache")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout_fixture(initialized: bool) -> tempfile::TempDir {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = fixture.path();
        fs::create_dir_all(root.join("crates/soldr-cli")).expect("crate directory");
        fs::write(
            root.join(MANIFEST),
            "[workspace.package]\nrepository = \"https://github.com/zackees/soldr\"\n",
        )
        .expect("root manifest");
        fs::write(
            root.join("crates/soldr-cli").join(MANIFEST),
            "[package]\nname = \"soldr-cli\"\n",
        )
        .expect("CLI manifest");
        fs::write(
            root.join(".gitmodules"),
            "[submodule \"_vender/zccache\"]\n\tpath = _vender/zccache\n",
        )
        .expect("gitmodules");
        if initialized {
            fs::create_dir_all(root.join("_vender/zccache")).expect("submodule directory");
            fs::write(root.join("_vender/zccache").join(MANIFEST), "[workspace]\n")
                .expect("submodule manifest");
        }
        fixture
    }

    #[test]
    fn missing_submodule_names_the_checkout_and_remedy() {
        let fixture = checkout_fixture(false);
        let missing =
            missing_zccache_submodule(&fixture.path().join("crates/soldr-cli"), &["build".into()]);
        assert_eq!(missing, Some(fixture.path().to_path_buf()));
    }

    #[test]
    fn initialized_submodule_is_accepted() {
        let fixture = checkout_fixture(true);
        assert_eq!(
            missing_zccache_submodule(fixture.path(), &["build".into()]),
            None
        );
    }

    #[test]
    fn unrelated_checkout_is_ignored() {
        let fixture = checkout_fixture(false);
        fs::write(
            fixture.path().join(MANIFEST),
            "[workspace.package]\nrepository = \"https://example.invalid/fork\"\n",
        )
        .expect("root manifest");
        assert_eq!(
            missing_zccache_submodule(fixture.path(), &["build".into()]),
            None
        );
    }

    #[test]
    fn commands_that_do_not_load_the_workspace_ignore_missing_submodule() {
        let fixture = checkout_fixture(false);
        for args in [
            vec!["--version".into()],
            vec!["new".into(), "demo".into()],
            vec!["locate-project".into()],
            vec!["install".into(), "serde".into()],
        ] {
            assert_eq!(missing_zccache_submodule(fixture.path(), &args), None);
        }
    }

    #[test]
    fn local_install_path_requires_submodule_but_external_path_does_not() {
        let fixture = checkout_fixture(false);
        let local_args = vec!["install".into(), "--path".into(), ".".into()];
        assert_eq!(
            missing_zccache_submodule(fixture.path(), &local_args),
            Some(fixture.path().to_path_buf())
        );

        let external = tempfile::tempdir().expect("external fixture");
        let external_args = vec![
            "install".into(),
            format!("--path={}", external.path().display()),
        ];
        assert_eq!(
            missing_zccache_submodule(fixture.path(), &external_args),
            None
        );
    }

    #[test]
    fn external_manifest_ignores_missing_submodule() {
        let fixture = checkout_fixture(false);
        let external = tempfile::tempdir().expect("external fixture");
        let manifest = external.path().join(MANIFEST);
        fs::write(
            &manifest,
            "[package]\nname = \"external\"\nversion = \"0.1.0\"\n",
        )
        .expect("external manifest");
        let args = vec![
            "test".into(),
            "--manifest-path".into(),
            manifest.display().to_string(),
        ];
        assert_eq!(missing_zccache_submodule(fixture.path(), &args), None);
    }
}
