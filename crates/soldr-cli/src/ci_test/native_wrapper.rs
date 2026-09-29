//! Selects the native Nextest wrapper for Fresh Nextest execution
//! (soldr#3453).
//!
//! `.config/nextest.toml` runs every Unix test through
//! `.github/scripts/nextest_wrapper.sh`, which execs the binary named in
//! [`NATIVE_WRAPPER_ENV`] when set and the Python wrapper otherwise. The
//! Python wrapper's interpreter start costs ~48 ms before each of ~3,500
//! tests; the native `soldr-nextest-wrapper` costs ~1-2 ms. CI installs it
//! beside the source-built `soldr` driver, so this stage finds it as a
//! sibling of the running executable. Linux only: it is the only host whose
//! wrapper contract (PDEATHSIG, ptrace authorization, cgroup ceiling, /proc
//! sampling and thread dumps) the native binary implements.

use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) const NATIVE_WRAPPER_ENV: &str = "SOLDR_NEXTEST_NATIVE_WRAPPER";
const NATIVE_WRAPPER_NAME: &str = "soldr-nextest-wrapper";

/// The native wrapper beside `exe`, when it exists.
pub(super) fn sibling_wrapper(exe: &Path) -> Option<PathBuf> {
    let candidate = exe.with_file_name(NATIVE_WRAPPER_NAME);
    candidate.is_file().then_some(candidate)
}

fn host_wrapper() -> Option<PathBuf> {
    let linux = crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux;
    if !linux {
        return None;
    }
    sibling_wrapper(&std::env::current_exe().ok()?)
}

/// Name the native wrapper for the `nextest` execution stage only. An
/// explicit caller value is left untouched.
pub(super) fn configure(command: &mut Command, stage_name: &str) {
    configure_with(command, stage_name, host_wrapper().as_deref());
}

pub(super) fn configure_with(command: &mut Command, stage_name: &str, wrapper: Option<&Path>) {
    if stage_name != "nextest" || std::env::var_os(NATIVE_WRAPPER_ENV).is_some() {
        return;
    }
    if let Some(wrapper) = wrapper {
        command.env(NATIVE_WRAPPER_ENV, wrapper);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(command: &Command) -> Option<PathBuf> {
        command
            .get_envs()
            .find(|(key, _)| *key == NATIVE_WRAPPER_ENV)
            .and_then(|(_, value)| value.map(PathBuf::from))
    }

    #[test]
    fn only_nextest_execution_is_pointed_at_the_native_wrapper() {
        if std::env::var_os(NATIVE_WRAPPER_ENV).is_some() {
            return; // an explicit caller value always wins.
        }
        let wrapper = Path::new("/drivers/soldr-nextest-wrapper");
        let mut execution = Command::new("unused");
        configure_with(&mut execution, "nextest", Some(wrapper));
        assert_eq!(named(&execution).as_deref(), Some(wrapper));

        for stage in ["nextest-compile", "doctests", "clippy"] {
            let mut other = Command::new("unused");
            configure_with(&mut other, stage, Some(wrapper));
            assert_eq!(named(&other), None, "{stage}");
        }
        let mut absent = Command::new("unused");
        configure_with(&mut absent, "nextest", None);
        assert_eq!(named(&absent), None);
    }

    #[test]
    fn the_wrapper_is_found_only_beside_the_driver() {
        let directory = tempfile::tempdir().expect("driver dir");
        let driver = directory.path().join("soldr");
        assert_eq!(sibling_wrapper(&driver), None);
        let wrapper = directory.path().join(NATIVE_WRAPPER_NAME);
        std::fs::write(&wrapper, b"wrapper").expect("write wrapper");
        assert_eq!(sibling_wrapper(&driver), Some(wrapper));
    }
}
