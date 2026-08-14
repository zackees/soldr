//! Source-checkout preflight for the vendored zccache dependency.

use std::fs;
use std::path::{Path, PathBuf};

use crate::core::SoldrError;

const RUST_MANIFEST_FILE: &str = concat!("C", "argo.toml");

pub(crate) fn ensure_zccache_submodule_initialized() -> Result<(), SoldrError> {
    let cwd = std::env::current_dir().map_err(|error| {
        SoldrError::Other(format!(
            "could not determine the current directory: {error}"
        ))
    })?;

    if let Some(checkout_root) = missing_zccache_submodule(&cwd) {
        return Err(SoldrError::Other(format!(
            "Soldr source checkout at {} is missing its vendored zccache submodule; run: git submodule update --init _vender/zccache",
            checkout_root.display(),
        )));
    }

    Ok(())
}

fn missing_zccache_submodule(cwd: &Path) -> Option<PathBuf> {
    for candidate in cwd.ancestors() {
        let manifest = candidate.join("_vender/zccache").join(RUST_MANIFEST_FILE);
        if is_soldr_source_checkout(candidate) && !manifest.is_file() {
            return Some(candidate.to_path_buf());
        }
    }

    None
}

fn is_soldr_source_checkout(candidate: &Path) -> bool {
    let root_manifest = fs::read_to_string(candidate.join(RUST_MANIFEST_FILE)).ok();
    let cli_manifest =
        fs::read_to_string(candidate.join("crates/soldr-cli").join(RUST_MANIFEST_FILE)).ok();
    let gitmodules = fs::read_to_string(candidate.join(".gitmodules")).ok();

    matches!(
        (root_manifest, cli_manifest, gitmodules),
        (Some(root), Some(cli), Some(modules))
            if root.contains("repository = \"https://github.com/zackees/soldr\"")
                && cli.contains("name = \"soldr-cli\"")
                && modules.lines().any(|line| line.trim() == "path = _vender/zccache")
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_zccache_submodule_initialized, is_soldr_source_checkout, missing_zccache_submodule,
        RUST_MANIFEST_FILE,
    };
    use std::fs;

    #[test]
    fn detects_missing_zccache_submodule_in_soldr_checkout() {
        let fixture = tempfile::tempdir().expect("fixture directory");
        let root = fixture.path();
        fs::create_dir_all(root.join("crates/soldr-cli")).expect("soldr crate directory");
        fs::write(
            root.join("Cargo.toml"),
            "[workspace.package]\nrepository = \"https://github.com/zackees/soldr\"\n",
        )
        .expect("root manifest");
        fs::write(
            root.join("crates/soldr-cli/Cargo.toml"),
            "[package]\nname = \"soldr-cli\"\n",
        )
        .expect("soldr cli manifest");
        fs::write(
            root.join(".gitmodules"),
            "[submodule \"_vender/zccache\"]\n\tpath = _vender/zccache\n",
        )
        .expect("gitmodules");

        assert!(is_soldr_source_checkout(root));
        assert_eq!(
            missing_zccache_submodule(&root.join("crates/soldr-cli")),
            Some(root.to_path_buf())
        );
    }

    #[test]
    fn reports_the_exact_submodule_remedy() {
        let _env = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = tempfile::tempdir().expect("fixture directory");
        let root = fixture.path();
        fs::create_dir_all(root.join("crates/soldr-cli/src")).expect("soldr crate directory");
        fs::write(
            root.join(RUST_MANIFEST_FILE),
            "[workspace.package]\nrepository = \"https://github.com/zackees/soldr\"\n",
        )
        .expect("root manifest");
        fs::write(
            root.join("crates/soldr-cli").join(RUST_MANIFEST_FILE),
            "[package]\nname = \"soldr-cli\"\n",
        )
        .expect("soldr cli manifest");
        fs::write(
            root.join(".gitmodules"),
            "[submodule \"_vender/zccache\"]\n\tpath = _vender/zccache\n",
        )
        .expect("gitmodules");
        let _cwd = crate::CwdGuard::enter(&root.join("crates/soldr-cli/src"));

        let error = ensure_zccache_submodule_initialized().expect_err("missing submodule error");
        assert!(error
            .to_string()
            .contains("git submodule update --init _vender/zccache"));
    }

    #[test]
    fn ignores_unrelated_missing_submodules() {
        let fixture = tempfile::tempdir().expect("fixture directory");
        let root = fixture.path();
        fs::create_dir_all(root.join("crates/soldr-cli")).expect("soldr crate directory");
        fs::write(
            root.join(RUST_MANIFEST_FILE),
            "[workspace.package]\nrepository = \"https://example.invalid/downstream\"\n",
        )
        .expect("root manifest");
        fs::write(
            root.join("crates/soldr-cli").join(RUST_MANIFEST_FILE),
            "[package]\nname = \"soldr-cli\"\n",
        )
        .expect("downstream manifest");
        fs::write(
            root.join(".gitmodules"),
            "[submodule \"_vender/zccache\"]\n\tpath = _vender/zccache\n",
        )
        .expect("gitmodules");

        assert_eq!(missing_zccache_submodule(root), None);
    }

    #[test]
    fn accepts_initialized_zccache_submodule() {
        let fixture = tempfile::tempdir().expect("fixture directory");
        let root = fixture.path();
        fs::create_dir_all(root.join("crates/soldr-cli")).expect("soldr crate directory");
        fs::create_dir_all(root.join("_vender/zccache")).expect("zccache directory");
        fs::write(
            root.join(RUST_MANIFEST_FILE),
            "[workspace.package]\nrepository = \"https://github.com/zackees/soldr\"\n",
        )
        .expect("root manifest");
        fs::write(
            root.join("crates/soldr-cli").join(RUST_MANIFEST_FILE),
            "[package]\nname = \"soldr-cli\"\n",
        )
        .expect("soldr cli manifest");
        fs::write(
            root.join(".gitmodules"),
            "[submodule \"_vender/zccache\"]\n\tpath = _vender/zccache\n",
        )
        .expect("gitmodules");
        fs::write(
            root.join("_vender/zccache").join(RUST_MANIFEST_FILE),
            "[workspace]\n",
        )
        .expect("zccache manifest");

        assert_eq!(missing_zccache_submodule(root), None);
    }
}
