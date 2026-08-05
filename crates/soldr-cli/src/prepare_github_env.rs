//! GitHub Actions environment-file export for `soldr prepare`.

use std::path::Path;

use crate::core::SoldrError;

pub(crate) fn append_env(path: Option<&Path>, key: &str, value: &str) -> Result<(), SoldrError> {
    if let Some(path) = path {
        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| SoldrError::Other(format!("open {}: {error}", path.display())))?;
        writeln!(file, "{key}={value}")
            .map_err(|error| SoldrError::Other(format!("write {}: {error}", path.display())))?;
    }
    Ok(())
}

pub(crate) fn apply_blessed_prep_env(
    github_env_path: Option<&Path>,
    prep: &crate::blessed_build::BlessedPrep,
) -> Result<(), SoldrError> {
    for (key, value) in crate::target_lifecycle::resolved_env(prep) {
        append_env(github_env_path, &key, &value)?;
        std::env::set_var(key, value);
    }
    // One target is exported at a time, so CMake can receive conventional
    // unscoped aliases in the next shell without polluting Soldr's process.
    if github_env_path.is_some() {
        for (source, alias) in [
            ("CMAKE_C_COMPILER", "CC"),
            ("CMAKE_CXX_COMPILER", "CXX"),
            ("CMAKE_AR", "AR"),
            ("CMAKE_RANLIB", "RANLIB"),
        ] {
            if let Some((_, value)) = prep.env.iter().find(|(key, _)| key == source) {
                append_env(github_env_path, alias, value)?;
            }
        }
    }
    if let Some(encoded) = crate::target_lifecycle::encoded_rustflags_for_prep(prep) {
        append_env(github_env_path, "CARGO_ENCODED_RUSTFLAGS", &encoded)?;
    }
    let mut path_dirs = prep.path_prefix();
    if !path_dirs.is_empty() {
        if let Some(current) = std::env::var_os("PATH") {
            path_dirs.extend(std::env::split_paths(&current));
        }
        let path_value = std::env::join_paths(path_dirs)
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|error| {
                SoldrError::Other(format!("failed to build prepared PATH: {error}"))
            })?;
        append_env(github_env_path, "PATH", &path_value)?;
        std::env::set_var("PATH", path_value);
    }
    if !prep.cargo_args.is_empty() {
        eprintln!(
            "soldr prepare: note: target uses Cargo --config syslib overrides; \
             `soldr build` applies those automatically"
        );
    }
    Ok(())
}
