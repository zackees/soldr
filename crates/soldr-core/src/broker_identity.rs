//! Installed broker executable identity shared by every soldr process role.
//!
//! The broker pipe is derived from this path. Compiler-named shims cannot use
//! their own `current_exe()` because that is the shim, not the broker install;
//! the front door therefore carries the exact path in the child environment.

use std::io;
use std::path::PathBuf;

/// Exact installed broker executable inherited by compiler shims and daemons.
pub const BROKER_EXECUTABLE_ENV_VAR: &str = "SOLDR_BROKER_EXECUTABLE";

/// Resolve the canonical installed broker executable.
///
/// There is deliberately no fallback after an explicit identity is present:
/// silently switching to `current_exe()` would make a wrapper shim dial a
/// different pipe. Without an explicit identity, a self-relocated soldr uses
/// `SOLDR_ORIGINAL_EXE`; an ordinary invocation uses its own executable.
pub fn installed_broker_executable() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(BROKER_EXECUTABLE_ENV_VAR) {
        return canonical_existing(PathBuf::from(path), BROKER_EXECUTABLE_ENV_VAR);
    }
    if let Some(path) = std::env::var_os(crate::self_relocate::ORIGINAL_EXE_ENV_VAR) {
        return canonical_existing(
            PathBuf::from(path),
            crate::self_relocate::ORIGINAL_EXE_ENV_VAR,
        );
    }
    let current = std::env::current_exe()?;
    canonical_existing(current, "current executable")
}

fn canonical_existing(path: PathBuf, source: &str) -> io::Result<PathBuf> {
    std::fs::canonicalize(&path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "cannot canonicalize installed broker executable from {source} at {}: {err}",
                path.display()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn explicit_missing_identity_has_no_current_exe_fallback() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let old = std::env::var_os(BROKER_EXECUTABLE_ENV_VAR);
        let missing =
            std::env::temp_dir().join(format!("soldr-missing-broker-{}", std::process::id()));
        std::env::set_var(BROKER_EXECUTABLE_ENV_VAR, &missing);
        let error = installed_broker_executable().expect_err("missing identity must fail");
        assert!(error.to_string().contains(BROKER_EXECUTABLE_ENV_VAR));
        match old {
            Some(value) => std::env::set_var(BROKER_EXECUTABLE_ENV_VAR, value),
            None => std::env::remove_var(BROKER_EXECUTABLE_ENV_VAR),
        }
    }
}
